//! Backtester module
//!
//! Plays back market data and executes strategies, providing a simulated broker and
//! account as well as statistics and data about the results of the strategy.

#![feature(conservative_impl_trait, associated_consts, custom_derive, proc_macro, test)]
#![allow(unused_variables, dead_code)]

extern crate algobot_util;
extern crate rand;
extern crate futures;
extern crate uuid;
extern crate redis;
extern crate postgres;
extern crate serde;
extern crate serde_json;
#[macro_use]
extern crate serde_derive;
extern crate test;

mod data;
mod conf;
mod backtest;
mod sim_broker;

use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::collections::HashMap;

use uuid::Uuid;
use futures::Future;
use futures::stream::{Stream, Receiver};
use serde_json::to_string;

use algobot_util::transport::command_server::{CommandServer, CsSettings};
use algobot_util::transport::redis::{sub_multiple, get_client};
use algobot_util::transport::commands::*;
use algobot_util::trading::tick::Tick;
use conf::CONF;
use backtest::*;
use data::*;
use sim_broker::*;

/// Starts the backtester module, initializing its interface to the rest of the platform
fn main() {
    let mut backtester = Backtester::new();
    backtester.listen();
}

/// What kind of method used to time the output of data
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BacktestType {
    Fast{delay_ms: usize},
    Live,
}

/// Where to get the data to drive the backtest
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum DataSource {
    Flatfile,
    RedisChannel{host: String, channel: String},
    Postgres,
    Random,
}

/// Where to send the backtest's generated data
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum DataDest {
    RedisChannel{host: String, channel: String},
    Console,
    Null,
    SimBroker{uuid: Uuid}, // Requires that a SimBroker is running on the Backtester in order to work
}

#[derive(Clone)]
struct Backtester {
    pub uuid: Uuid,
    pub cs: CommandServer,
    pub running_backtests: Arc<Mutex<HashMap<Uuid, BacktestHandle>>>,
    pub simbrokers: Arc<Mutex<HashMap<Uuid, SimBroker>>>,
}

impl Backtester {
    pub fn new() -> Backtester {
        let settings = CsSettings {
            conn_count: 2,
            redis_host: CONF.redis_url,
            responses_channel: CONF.redis_responses_channel,
            timeout: 2020,
            max_retries: 3,
        };

        let uuid = Uuid::new_v4();

        Backtester {
            uuid: uuid,
            cs: CommandServer::new(settings),
            running_backtests: Arc::new(Mutex::new(HashMap::new())),
            simbrokers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Creates a SimBroker that's managed by the Backtester.  Returns its UUID.
    pub fn init_simbroker(&mut self, settings: SimBrokerSettings) -> Uuid {
        let mut simbrokers = self.simbrokers.lock().unwrap();
        let simbroker = SimBroker::new(settings);
        let uuid = Uuid::new_v4();
        simbrokers.insert(uuid, simbroker);
        uuid
    }

    /// Starts listening for commands from the rest of the platform
    pub fn listen(&mut self) {
        // subscribe to the command channels
        let rx = sub_multiple(
            CONF.redis_url, &[CONF.redis_control_channel, self.uuid.hyphenated().to_string().as_str()]
        );
        let mut redis_client = get_client(CONF.redis_url);
        let mut copy = self.clone();

        // Signal to the platform that we're ready to receive commands
        let _ = send_command(&WrappedCommand::from_command(
            Command::Ready{instance_type: "Backtester".to_string(), uuid: self.uuid}), &mut redis_client, "control"
        );

        for res in rx.wait() {
            let (_, msg) = res.unwrap();
            let wr_cmd = match WrappedCommand::from_str(msg.as_str()) {
                Ok(wr) => wr,
                Err(e) => {
                    println!("Unable to parse WrappedCommand from String: {:?}", e);
                    return;
                }
            };

            let res: Response = match wr_cmd.cmd {
                Command::Ping => Response::Pong{ args: vec![copy.uuid.hyphenated().to_string()] },
                Command::Type => Response::Info{ info: "Backtester".to_string() },
                Command::StartBacktest{definition: definition_str} => {
                    let definition = serde_json::from_str(definition_str.as_str());
                    if definition.is_err() {
                        let err_msg = definition.err().unwrap();
                        Response::Error{
                            status: format!("Can't parse backtest defition from String: {}", err_msg)
                        }
                    } else {
                        // start the backtest and register a handle internally
                        let uuid = copy.start_backtest(definition.unwrap());

                        match uuid {
                            Ok(uuid) => Response::Info{info: uuid.hyphenated().to_string()},
                            Err(err) => Response::Error{status: err}
                        }
                    }
                },
                Command::Kill => {
                    thread::spawn(|| {
                        thread::sleep(std::time::Duration::from_secs(3));
                        std::process::exit(0);
                    });

                    Response::Info{info: "Backtester will self-destruct in 3 seconds.".to_string()}
                }
                Command::PauseBacktest{uuid} => {
                    match copy.send_backtest_cmd(&uuid, BacktestCommand::Pause) {
                        Ok(()) => Response::Ok,
                        Err(()) => Response::Error{status: "No backtest with that uuid!".to_string()},
                    }
                },
                Command::ResumeBacktest{uuid} => {
                    match copy.send_backtest_cmd(&uuid, BacktestCommand::Resume) {
                        Ok(()) => Response::Ok,
                        Err(()) => Response::Error{status: "No backtest with that uuid!".to_string()},
                    }
                },
                Command::StopBacktest{uuid} => {
                    match copy.send_backtest_cmd(&uuid, BacktestCommand::Stop) {
                        Ok(()) => {
                            // deregister from internal running backtest list
                            copy.remove_backtest(&uuid);
                            Response::Ok
                        },
                        Err(()) => Response::Error{status: "No backtest with that uuid!".to_string()},
                    }
                },
                Command::ListBacktests => {
                    let backtests = copy.running_backtests.lock().unwrap();
                    let mut message_vec = Vec::new();
                    for (uuid, backtest) in backtests.iter() {
                        let ser_handle = SerializableBacktestHandle::from_handle(backtest, *uuid);
                        message_vec.push(ser_handle);
                    }

                    let message = to_string(&message_vec);
                    match message {
                        Ok(msg) => Response::Info{ info: msg },
                        Err(e) => Response::Error{ status: "Unable to convert backtest list into String.".to_string() },
                    }
                },
                Command::SpawnSimbroker{settings} => {
                    let uuid = copy.init_simbroker(settings);
                    Response::Info{info: uuid.hyphenated().to_string()}
                },
                Command::ListSimbrokers => {
                    let simbrokers = copy.simbrokers.lock().unwrap();
                    let mut uuids = Vec::new();
                    for (uuid, _) in simbrokers.iter() {
                        uuids.push(uuid.hyphenated().to_string());
                    }
                    let message = serde_json::to_string(&uuids).unwrap();
                    Response::Info{info: message}
                },
                _ => Response::Error{ status: "Backtester doesn't recognize that command.".to_string() }
            };

            redis::cmd("PUBLISH")
                .arg(CONF.redis_responses_channel)
                .arg(res.wrap(wr_cmd.uuid).to_string().unwrap().as_str())
                .execute(&mut redis_client);
            // TODO: Test to make sure this actually works
        }
    }

    /// Initiates a new backtest and adds it to the internal list of monitored backtests.
    fn start_backtest(
        &mut self, definition: BacktestDefinition) -> Result<Uuid, String>
    {
        println!("Starting backtest: ");
        println!("{:?}", definition);
        // Create the TickGenerator that provides the backtester with data
        let mut src: Box<TickGenerator> = resolve_data_source(
            &definition.data_source, definition.symbol.clone(), definition.start_time
        );

        // create channel for communicating messages to the running backtest sent externally
        let (external_handle_tx, handle_rx) = mpsc::sync_channel::<BacktestCommand>(5);
        // create channel for communicating messages to the running backtest internally
        let internal_handle_tx = external_handle_tx.clone();

        // modify the source tickstream to add delay between the ticks or add some other kind of
        // advanced functionality to the way they're outputted
        let tickstream: Result<Receiver<Tick, ()>, String> = match &definition.backtest_type {
            &BacktestType::Fast{delay_ms} => src.get(
                Box::new(FastMap{delay_ms: delay_ms}), handle_rx
            ),
            &BacktestType::Live => src.get(Box::new(LiveMap::new()), handle_rx),
        };

        if tickstream.is_err() {
            return Err( format!("Error creating tickstream: {}", tickstream.err().unwrap()) )
        }

        // create a TickSink that receives the output of the backtest
        let dst_opt: Result<Box<TickSink + Send>, Uuid> = match &definition.data_dest {
            &DataDest::RedisChannel{ref host, ref channel} => {
                Ok(Box::new(RedisSink::new(definition.symbol.clone(), channel.clone(), host.as_str())))
            },
            &DataDest::Console => Ok(Box::new(ConsoleSink{})),
            &DataDest::Null => Ok(Box::new(NullSink{})),
            &DataDest::SimBroker{uuid} => Err(uuid),
        };

        let _definition = definition.clone();
        let mut i = 0;
        let uuid = Uuid::new_v4();

        // initiate tick flow
        if dst_opt.is_ok() {
            let mut dst = dst_opt.unwrap();
            let _ = tickstream.unwrap().for_each(move |t| {
                i += 1;

                // send the tick to the sink
                dst.tick(t);

                if check_early_exit(&t, &_definition, i) {
                    println!("Backtest exiting early.");
                    return Err(())
                }

                Ok(())
            }).or_else(move |_| {
                println!("Stopping backtest because tickstream has ended");
                let _ = internal_handle_tx.send(BacktestCommand::Stop);
                Ok::<(), ()>(())
            }).forget();
        } else {
            let mut simbrokers = self.simbrokers.lock().unwrap();
            let simbroker_opt = simbrokers.get_mut(&dst_opt.err().unwrap());
            if simbroker_opt.is_none() {
                return Err("No SimBroker running with that Uuid!".to_string())
            }

            let simbroker = simbroker_opt.unwrap();
            // plug the tickstream into the matching SimBroker
            simbroker.register_tickstream(definition.symbol.clone(), tickstream.unwrap()).unwrap();
        }

        let handle = BacktestHandle {
            symbol: definition.symbol,
            backtest_type: definition.backtest_type,
            data_source: definition.data_source,
            endpoint: definition.data_dest,
            handle: external_handle_tx
        };

        // register the backtest's existence
        let mut backtest_list = self.running_backtests.lock().unwrap();
        backtest_list.insert(uuid, handle);

        Ok(uuid)
    }

    /// Removes a stopped backtest from the internal running backtest list
    pub fn remove_backtest(&mut self, uuid: &Uuid) {
        let mut handles = self.running_backtests.lock().unwrap();
        handles.remove(&uuid);
    }

    /// Sends a command to a managed backtest
    pub fn send_backtest_cmd(&mut self, uuid: &Uuid, cmd: BacktestCommand) -> Result<(), ()> {
        let handles = self.running_backtests.lock().unwrap();
        let handle = handles.get(&uuid);

        if handle.is_none() {
            return Err(());
        }
        let ref sender = handle.unwrap().handle;
        let _ = sender.send(cmd);

        Ok(())
    }
}

/// Creates a TickGenerator from a DataSource and symbol String
pub fn resolve_data_source(data_source: &DataSource, symbol: String, start_time: Option<usize>) -> Box<TickGenerator> {
    match data_source {
        &DataSource::Flatfile => {
            Box::new(FlatfileReader{
                symbol: symbol.clone(),
                start_time: start_time,
            }) as Box<TickGenerator>
        },
        &DataSource::RedisChannel{ref host, ref channel} => {
            Box::new(
                RedisReader::new(symbol.clone(), host.clone(), channel.clone())
            ) as Box<TickGenerator>
        },
        &DataSource::Random => {
            Box::new(RandomReader::new(symbol.clone())) as Box<TickGenerator>
        },
        &DataSource::Postgres => {
            Box::new(PostgresReader {symbol: symbol, start_time: start_time} )
        },
    }
}

/// Returns true if the backtest has met a stop condition.
fn check_early_exit (
    t: &Tick, def: &BacktestDefinition, i: usize
) -> bool {
    if def.max_tick_n.is_some() &&
       def.max_tick_n.unwrap() <= i {
        return true
    } else if def.max_timestamp.is_some() &&
              def.max_timestamp.unwrap() <= t.timestamp {
        return true
    }

    false
}

#[test]
fn backtest_n_early_exit() {
    let rx = algobot_util::transport::redis::sub_channel(CONF.redis_url, "test1_ii");

    let mut bt = Backtester::new();
    let definition = BacktestDefinition {
        start_time: None,
        max_tick_n: Some(10),
        max_timestamp: None,
        symbol: "TEST".to_string(),
        backtest_type: BacktestType::Fast{delay_ms: 0},
        data_source: DataSource::Random,
        data_dest: DataDest::RedisChannel{
            host: CONF.redis_url.to_string(),
            channel: "test1_ii".to_string()
        },
        broker_settings: SimBrokerSettings::default(),
    };

    let uuid = bt.start_backtest(definition).unwrap();
    // backtest starts paused so resume it
    let _ = bt.send_backtest_cmd(&uuid, BacktestCommand::Resume);
    let res = rx.wait().take(10).collect::<Vec<_>>();
    assert_eq!(res.len(), 10);
}

#[test]
fn backtest_timestamp_early_exit() {
    let rx = algobot_util::transport::redis::sub_channel(CONF.redis_url, "test2_ii");

    let mut bt = Backtester::new();
    let definition = BacktestDefinition {
        start_time: None,
        max_tick_n: None,
        max_timestamp: Some(8),
        symbol: "TEST".to_string(),
        backtest_type: BacktestType::Fast{delay_ms: 0},
        data_source: DataSource::Random,
        data_dest: DataDest::RedisChannel{
            host: CONF.redis_url.to_string(),
            channel: "test2_ii".to_string()
        },
        broker_settings: SimBrokerSettings::default(),
    };

    let uuid = bt.start_backtest(definition).unwrap();
    // backtest starts paused so resume it
    let _ = bt.send_backtest_cmd(&uuid, BacktestCommand::Resume);
    let res = rx.wait().take(8).collect::<Vec<_>>();
    assert_eq!(res.len(), 8);
}
