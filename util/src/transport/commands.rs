//! Contains the definitions of all commands in the Command intermodular communication
//! system as well as helper functions for Serialization/Deserialization and unwrapping.

use serde_json;
use redis;
use uuid::Uuid;
#[allow(unused_imports)]
use test;

use trading::broker::SimBrokerSettings;

/// Represents a Command that can be serde'd and sent over Redis.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub enum Command {
    // Generic Commands; all instances must implement responses for these commands.
    Ping,
    Shutdown,
    Kill,
    Register{channel: String},
    Type, // returns what kind of instance this is
    Ready{instance_type: String, uuid: Uuid}, // signals that a newly spawned instance is ready to receive commands
    // Tick Parser Commands
    AddSMA{period: usize},
    RemoveSMA{period: usize},
    // Spawner Commands
    SpawnMM,
    Census,
    SpawnOptimizer{strategy: String},
    SpawnTickParser{symbol: String},
    SpawnBacktester,
    SpawnFxcmDataDownloader,
    KillInstance{uuid: Uuid},
    KillAllInstances,
    // Backtester Commands
    StartBacktest{definition: String},
    PauseBacktest{uuid: Uuid},
    ResumeBacktest{uuid: Uuid},
    StopBacktest{uuid: Uuid},
    ListBacktests,
    ListSimbrokers,
    SpawnSimbroker{settings: SimBrokerSettings},
    // Data Downloader Commands
    DownloadTicks{start_time: String, end_time: String, symbol: String, dst: HistTickDst},
    ListRunningDownloads,
    DownloadComplete{start_time: String, end_time: String, symbol: String, dst: HistTickDst},
}

/// Represents a response from the Tick Processor to a Command sent
/// to it at some earlier point.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub enum Response {
    // Generic Responses
    Ok,
    Error{status: String},
    Pong{args: Vec<String>},
    Info{info: String}
}

impl Command {
    pub fn from_str(raw: &str) -> Result<Command, ()> {
        serde_json::from_str(raw).map_err(|_| { () } )
    }

    pub fn to_string(&self) -> Result<String, ()> {
        serde_json::to_string(self).map_err(|_| { () } )
    }

    /// Generates a new Uuid and creates a new WrappedCommand
    pub fn wrap(&self) -> WrappedCommand {
        WrappedCommand {
            uuid: Uuid::new_v4(),
            cmd: self.clone()
        }
    }
}

/// Where to save the recorded ticks to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum HistTickDst {
    Flatfile{filename: String},
    Postgres{table: String},
    RedisChannel{host: String, channel: String},
    RedisSet{host: String, set_name: String},
    Console,
}

/// Represents a command bound to a unique identifier that can be
/// used to link it with a Response
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct WrappedCommand {
    pub uuid: Uuid,
    pub cmd: Command
}

impl WrappedCommand {
    pub fn from_str(raw: &str) -> Result<WrappedCommand, ()> {
        serde_json::from_str(raw).map_err(|_| { () } )
    }

    pub fn to_string(&self) -> Result<String, ()> {
        serde_json::to_string(self).map_err(|_| { () } )
    }

    /// Creates a new WrappedCommand with the given command as an inner
    pub fn from_command(cmd: Command) -> WrappedCommand {
        WrappedCommand {
            uuid: Uuid::new_v4(),
            cmd: cmd.clone()
        }
    }
}

/// Converts a String into a WrappedCommand
/// JSON Format: {"uuid": "xxxx-xxxx", "cmd": {"CommandName":{"arg": "val"}}}
pub fn parse_wrapped_command(raw: String) -> WrappedCommand {
    let res = serde_json::from_str::<WrappedCommand>(raw.as_str());
    match res {
        Ok(wr_cmd) => return wr_cmd,
        Err(_) => panic!("Unable to parse WrappedCommand from String: {}", raw)
    }
}

impl Response {
    pub fn from_str(raw: &str) -> Result<Response, ()> {
        serde_json::from_str(raw).map_err(|_| { () } )
    }

    pub fn to_string(&self) -> Result<String, ()> {
        serde_json::to_string(self).map_err(|_| { () } )
    }

    /// Creates a new WrappedResponse from a Command and a Uuid
    pub fn wrap(&self, uuid: Uuid) -> WrappedResponse {
        WrappedResponse {
            uuid: uuid,
            res: self.clone()
        }
    }
}

/// A Response bound to a UUID
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct WrappedResponse {
    pub uuid: Uuid,
    pub res: Response
}

impl WrappedResponse {
    pub fn from_str(raw: &str) -> Result<WrappedResponse, ()> {
        serde_json::from_str(raw).map_err(|_| { () } )
    }

    pub fn to_string(&self) -> Result<String, ()> {
        serde_json::to_string(self).map_err(|_| { () } )
    }

    /// Creates a new WrappedResponse from a Response and a Uuid
    pub fn from_response(res: Response, uuid: Uuid) -> WrappedResponse {
        WrappedResponse {
            uuid: uuid,
            res: res
        }
    }
}

/// Utility function to asynchronously sends off a command
pub fn send_command(cmd: &WrappedCommand, client: &redis::Client, commands_channel: &str) -> Result<(), serde_json::Error> {
    let command_string = try!(serde_json::to_string(cmd));
    redis::cmd("PUBLISH")
        .arg(commands_channel)
        .arg(command_string)
        .execute(client);
    Ok(())
}

/// Utility function to asynchronously send off a response
pub fn send_response(res: &WrappedResponse, client: &redis::Client, channel: &str) -> Result<(), serde_json::Error> {
    let ser = try!(serde_json::to_string(res));
    let res_str = ser.as_str();
    let _ = redis::cmd("PUBLISH")
        .arg(channel)
        .arg(res_str)
        .execute(client);
    Ok(())
}

/// Parses a String into a WrappedResponse
///
/// Left in for backwards compatability
pub fn parse_wrapped_response(raw_res: String) -> WrappedResponse {
    serde_json::from_str::<WrappedResponse>(raw_res.as_str())
        .expect("Unable to parse WrappedResponse from String")
}

#[test]
fn command_serialization() {
    let cmd_str = "{\"AddSMA\": {\"period\": 664} }";
    let cmd: Command = serde_json::from_str(cmd_str).unwrap();
    assert_eq!(cmd, Command::AddSMA{period: 664});
}

#[test]
fn command_deserialization() {
    let cmd = Command::RemoveSMA{period: 664};
    let cmd_string = serde_json::to_string(&cmd).unwrap();
    assert_eq!("{\"RemoveSMA\":{\"period\":664}}", cmd_string.as_str());
}

#[test]
fn response_serialization() {
    let res_str = "\"Ok\"";
    let res: Response = serde_json::from_str(res_str).unwrap();
    assert_eq!(res, Response::Ok);
}

#[test]
fn response_deserialization() {
    let res = Response::Ok;
    let res_string = serde_json::to_string(&res).unwrap();
    assert_eq!("\"Ok\"", res_string.as_str());
}

#[bench]
fn wrappedcmd_to_string(b: &mut test::Bencher) {
    let cmd = Command::AddSMA{period: 4223423};
    let wr_cmd = WrappedCommand{uuid: Uuid::new_v4(), cmd: cmd};
    b.iter(|| {
        let wr_cmd = &wr_cmd;
        let _ = serde_json::to_string(wr_cmd);
    })
}

#[bench]
fn string_to_wrappedcmd(b: &mut test::Bencher) {
    let raw = "{\"uuid\":\"2f663301-5b73-4fa0-b201-09ab196ec5fd\",\"cmd\":{\"RemoveSMA\":{\"period\":5.2342}}}";
    b.iter(|| {
        let raw = &raw;
        let _: WrappedCommand  = serde_json::from_str(raw).unwrap();
    })
}

#[bench]
fn uuid_generation(b: &mut test::Bencher) {
    b.iter(|| {
        Uuid::new_v4();
    })
}
