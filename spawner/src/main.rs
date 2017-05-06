//! Bot4 Instance Spawner and Manager
//!
//! Responsible for spawning, destroying, and managing all instances of the bot4
//! platform's modules and reporting on their status.

#![feature(plugin, test, conservative_impl_trait, custom_derive, proc_macro)]

extern crate uuid;
extern crate redis;
extern crate algobot_util;
extern crate futures;
extern crate test;
extern crate serde;
extern crate serde_json;
#[macro_use]
extern crate serde_derive;

mod conf;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::process;

use conf::CONF;

use uuid::Uuid;
use futures::{Future, oneshot, Complete};
use futures::stream::Stream;
#[allow(unused_imports)]
use algobot_util::transport::redis::{sub_channel, sub_multiple, get_client};
use algobot_util::transport::commands::*;
use algobot_util::transport::command_server::*;

/// Represents an instance of a platform module.  Contains a Uuid to identify it
/// as well as some information about its spawning parameters and its type.
#[derive(Serialize, Debug, Clone)]
struct Instance {
    // TODO: Spawning parameters
    instance_type: String,
    uuid: Uuid
}

/// Holds a list of all instances that the spawner has spawned and thinks are alive
#[derive(Clone)]
struct InstanceManager {
    pub uuid: Uuid,
    pub living: Arc<Mutex<Vec<Instance>>>,
    pub cs: CommandServer
}

impl InstanceManager {
    /// Creates a new spawner instance.
    pub fn new() -> InstanceManager {
        InstanceManager {
            uuid: Uuid::new_v4(),
            living: Arc::new(Mutex::new(Vec::new())),
            cs: CommandServer::new(get_settings())
        }
    }

    /// Starts listening for commands on the control channel, spawns a new MM instance,
    /// and initializes the ping heartbeat.
    pub fn init(&mut self) {
        // spawn a MM instance
        self.spawn_mm();

        // find any disconnected instances
        let stragglers = self.ping_all().wait().unwrap().unwrap();

        if CONF.kill_stragglers {
            for straggler_response in stragglers {
                match straggler_response {
                    Response::Pong{args} => {
                        if args.len() < 1 {
                            println!("Malformed Pong received: {:?}", args);
                        } else {
                            println!("Sending Kill message to straggler with uuid {:?}", args[0]);
                            self.cs.execute(
                                Command::Kill,
                                args[0].clone()
                            );
                        }
                    },
                    _ => {
                        println!("Unrecognized response received: {:?}", straggler_response);
                    }
                }
            }
        } else {
            // TODO
            unimplemented!();
        }

        // listen for new commands and setup callbacks
        // important to do this AFTER dealing with stragglers, or else we may attempt suicide.
        self.listen();

        // give CommandServer a while to boot up
        thread::sleep(Duration::from_millis(1928));

        // register self as in instance
        {
            let mut living = self.living.lock().unwrap();
            (*living).push(Instance{instance_type: "Spawner".to_string(), uuid: self.uuid});
        }

        // start ping heartbeat
        loop {
            // blocks until all instances return their expected responses or time out
            let responses = self.ping_all().wait().ok().unwrap().unwrap();

            let dead_uuid_outer = self.get_missing_instance(responses);
            if dead_uuid_outer.is_some() {
                let dead_instance = dead_uuid_outer.unwrap();
                println!("Instance {:?} is unresponseive; attempting respawn", dead_instance);

                // deregister the old instance
                self.remove_instance(dead_instance.uuid);

                let res_outer = self.cs.execute(
                    Command::Type,
                    dead_instance.uuid.hyphenated().to_string()
                ).wait().unwrap();

                match res_outer {
                    Ok(response) => { // we actually got a reply from the presumed dead instance
                        match response {
                            Response::Info{info} => {
                                println!("{:?} wasn't dead after all...", dead_instance);
                                self.add_instance(Instance{instance_type: info, uuid: dead_instance.uuid});
                            },
                            _ => {
                                println!("Received unexpected response from Type query: {:?}", response);
                            }
                        }
                    },
                    Err(_) => {
                        println!("{:?} is really, truly, dead.", dead_instance);
                        // TODO: respawn dead instance
                    }
                }
            }

            thread::sleep(Duration::from_millis(350));
        }
    }

    /// Returns the uuid of the first missing instance
    fn get_missing_instance(&self, responses: Vec<Response>) -> Option<Instance> {
        let assumed_living = self.living.lock().unwrap();

        // check to make sure that each expected instance is in the responses
        for inst in (*assumed_living).iter() {
            let mut present = false;
            for res in responses.iter() {
                match res {
                    &Response::Pong{ref args} => {
                        if args.len() < 1 {
                            println!("Malformed Pong received: {:?}", args);
                        } else if inst.uuid.hyphenated().to_string() == args[0] {
                            present = true;
                            break;
                        }
                    },
                    _ => {
                        println!("Received unexpected response to Ping: {:?}", res);
                    }
                }
            }

            if !present {
                let temp_inst = inst.clone();
                return Some(temp_inst)
            }
        }

        None
    }

    /// Starts listening for new commands on the control channel
    pub fn listen(&mut self) {
        let mut dup = self.clone();
        let own_uuid = self.uuid.clone();

        thread::spawn(move || {
            // sub to spawer control channel and personal commands channel
            let cmds_rx = sub_multiple(
                CONF.redis_url,
                &[CONF.redis_control_channel, own_uuid.hyphenated().to_string().as_str()]
            );
            println!(
                "Listening for commands on {} and {}",
                CONF.redis_control_channel,
                own_uuid.hyphenated().to_string().as_str()
            );
            let mut redis_client = get_client(CONF.redis_url);

            let _ = cmds_rx.for_each(move |message| {
                let (_, cmd_string) = message;

                match WrappedCommand::from_str(cmd_string.as_str()) {
                    Ok(wr_cmd) => {
                        let (c, o) = oneshot::<Response>();
                        dup.handle_command(wr_cmd.cmd, c);

                        let uuid = wr_cmd.uuid.clone();
                        let _ = o.and_then(|status: Response| {
                            redis::cmd("PUBLISH")
                                .arg(CONF.redis_responses_channel)
                                .arg(status.wrap(uuid).to_string().unwrap().as_str())
                                .execute(&mut redis_client);
                            Ok(())
                        }).wait();
                    },
                    Err(_) => {
                        println!("Couldn't parse WrappedCommand from: {:?}", cmd_string);
                    },
                }

                Ok(())
            }).wait();
        });
    }

    /// Processes an incoming command, doing whatever it instructs and fulfills the future
    /// that it fulfills with the status once it's finished.
    fn handle_command(&mut self, cmd: Command, c: Complete<Response>) {
        let res = match cmd {
            Command::Ping => Response::Pong{args: vec![self.uuid.hyphenated().to_string()]},
            Command::Kill => {
                thread::spawn(||{
                    // blow up after 3 seconds
                    thread::sleep(Duration::new(3, 0));
                    println!("This is the end...");
                    std::process::exit(0);
                });
                Response::Info{info: "Shutting down in 3 seconds...".to_string()}
            }
            Command::Type => Response::Info{info: "Spawner".to_string()},
            // This means a new instance has spawned and we should register it in our internal instance list
            Command::Ready{instance_type, uuid} => {
                self.add_instance(Instance{instance_type: instance_type, uuid: uuid});
                Response::Ok
            },
            Command::KillAllInstances => self.kill_all(),
            Command::Census => self.census(),
            Command::SpawnMM => self.spawn_mm(),
            Command::SpawnOptimizer{strategy} => self.spawn_optimizer(strategy),
            Command::SpawnTickParser{symbol} => self.spawn_tick_parser(symbol),
            Command::SpawnBacktester => self.spawm_backtester(),
            Command::SpawnFxcmDataDownloader => self.spawn_fxcm_dd(),
            _ => Response::Error{
                status: "Command not accepted by the instance spawner".to_string()
            }
        };

        c.complete(res);
    }

    /// Returns a list of all living instances
    fn census(&self) -> Response {
        let living = self.living.lock().unwrap();
        let mut partials = Vec::new();
        for inst in living.iter() {
            match serde_json::to_string(inst) {
                Ok(ser) => partials.push(ser),
                Err(e) => return Response::Error{
                    status: format!("Error serializing instance: {:?}", e)
                }
            }
        }

        let res_string = format!("[{}]", partials.join(", "));
        return Response::Info{info: res_string};
    }

    /// Spawns a new MM server instance and inserts its Uuid into the living instances list
    fn spawn_mm(&mut self) -> Response {
        let mod_uuid = Uuid::new_v4();
        let path = CONF.dist_path.to_string() + "mm/manager.js";
        let _ = process::Command::new(CONF.node_binary_path)
                                .arg(path)
                                .arg(mod_uuid.to_string().as_str())
                                .spawn()
                                .expect("Unable to spawn MM");

        Response::Ok
    }

    /// Spawns a new Tick Processor instance with the given symbol andinserts its Uuid into
    /// the living instances list
    fn spawn_tick_parser(&mut self, symbol: String) -> Response {
        let mod_uuid = Uuid::new_v4();
        let path = CONF.dist_path.to_string() + "tick_processor";
        let _ = process::Command::new(path)
                                .arg(mod_uuid.to_string().as_str())
                                .arg(symbol.as_str())
                                .spawn()
                                .expect("Unable to spawn Tick Parser");

        Response::Ok
    }

    /// Spawns a new Optimizer instance with the specified strategy andinserts its Uuid into
    /// the living instances list
    fn spawn_optimizer(&mut self, strategy: String) -> Response {
        let mod_uuid = Uuid::new_v4();
        let path = CONF.dist_path.to_string() + "optimizer";
        let _ = process::Command::new(path)
                                .arg(mod_uuid.to_string().as_str())
                                .arg(strategy.as_str())
                                .spawn()
                                .expect("Unable to spawn Optimizer");

        Response::Ok
    }

    /// Spawns a Backtester instance.
    fn spawm_backtester(&mut self) -> Response {
        let mod_uuid = Uuid::new_v4();
        let path = CONF.dist_path.to_string() + "backtester";
        let _ = process::Command::new(path)
                                .arg(mod_uuid.to_string().as_str())
                                .spawn()
                                .expect("Unable to spawn Optimizer");

        Response::Ok
    }

    /// Spawns a FXCM Data Downloader instance.
    fn spawn_fxcm_dd(&mut self) -> Response {
        let mod_uuid = Uuid::new_v4();
        let path = CONF.dist_path.to_string() + "fxcm_native_downloader";
        let _ = process::Command::new(path)
                                .arg(mod_uuid.to_string().as_str())
                                .spawn()
                                .expect("Unable to spawn FXCM Data Downloader");

        Response::Ok
    }

    /// Broadcasts a Ping message on the broadcast channel to all running instances.  Returns
    /// a future that fulfills to a Vec containing the uuids of all running instances.
    fn ping_all(&mut self) -> impl Future<Item = Result<Vec<Response>, String>, Error = futures::Canceled> {
        self.cs.broadcast(
            Command::Ping,
            CONF.redis_control_channel.to_string()
        )
    }

    /// Kills all currently running instances managed by this spawner
    fn kill_all(&mut self) -> Response {
        // TODO: Maybe make this actually verify the responses before returning Ok.
        let mut instances_inner = self.living.lock().unwrap();
        for inst in instances_inner.drain(..) {
            let prom = self.cs.execute(Command::Kill, inst.uuid.hyphenated().to_string());
            prom.and_then(|response| {
                println!("{:?}", response);
                Ok(())
            }).forget();
        }

        Response::Ok
    }

    /// Adds an instance to the internal living instances list
    fn add_instance(&self, inst: Instance) {
        let l = self.living.clone();
        let mut ll = l.lock().unwrap();
        ll.push(inst);
    }

    /// Removes an instance with the given Uuid from the internal instances list
    fn remove_instance(&self, uuid: Uuid) {
        let l = self.living.clone();
        let mut ll = l.lock().unwrap();

        let mut _i: Option<usize> = None;
        for (i, inst) in (*ll).iter().enumerate() {
            if inst.uuid == uuid {
                _i = Some(i);
            }
        }

        if _i.is_some() {
            ll.remove(_i.unwrap());
        }
    }
}

fn get_settings() -> CsSettings {
    CsSettings {
        redis_host: CONF.redis_url,
        responses_channel: CONF.redis_responses_channel,
        conn_count: 3,
        timeout: 300,
        max_retries: 3
    }
}

fn main() {
    let mut spawner = InstanceManager::new();
    spawner.init();
}

/// Tests the instance manager's ability to process incoming Commands.
#[test]
fn spawner_command_processing() {
    let mut spawner = InstanceManager::new();
    spawner.listen();

    let mut client = get_client(CONF.redis_url);
    let cmd = Command::Ping.wrap();
    let cmd_string = cmd.to_string().unwrap();

    let rx = sub_channel(CONF.redis_url, CONF.redis_responses_channel);
    // give the sub a chance to subscribe
    thread::sleep(Duration::from_millis(150));
    // send a Ping command
    redis::cmd("PUBLISH")
        .arg(spawner.uuid.hyphenated().to_string())
        .arg(cmd_string.as_str())
        .execute(&mut client);

    // Wait for a Pong to be received
    let res = rx.wait().next().unwrap().unwrap();
    assert_eq!(
        WrappedResponse::from_str(res.as_str()).unwrap().res,
        Response::Pong{args: vec![spawner.uuid.hyphenated().to_string()]}
    );
}

// #[test]
            // disabled until relative pathing implemented TODO
// fn tick_processor_spawning() {
//     let mut spawner = InstanceManager::new();
//     spawner.spawn_tick_parser("_test3".to_string());

//     let living = spawner.living.clone();
//     let spawned_uuid: Uuid;
//     {
//         let living_inner = living.lock().unwrap();
//         assert_eq!((*living_inner).len(), 1);
//         spawned_uuid = living_inner[0].uuid.clone();
//     }

//     let mut cs = CommandServer::new(get_settings());
//     cs.execute(Command::Kill, spawned_uuid.hyphenated().to_string());
// }
