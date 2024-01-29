//! The `repl` module contains the code for handling the read-eval-print-loop (REPL) for the
//! client of the file sharing tool.

use crate::DynResult;

use libp2p::PeerId;
use log::error;
use std::io::{stdout, Write};
use std::str::FromStr;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc::Sender, oneshot};

/// The responses that the command core can send back to the [Repl].
#[derive(Debug)]
pub enum QueryResult {
    /// A list of file names.
    Files(Vec<String>),
    Boolean(bool),
}

/// The commands that the [Repl] can send to the command core.
#[derive(Debug)]
pub enum ReplCommand {
    /// Manually dial the given peer. The second arg is the transmission channel to send the dial result.
    Dial(PeerId, oneshot::Sender<DynResult<()>>),

    /// Get the list of peers. The second arg is the transmission channel to send the peer list.
    GetPeerList(oneshot::Sender<Vec<PeerId>>),

    /// The peer id of this peer. The second arg is the transmission channel to send the peer id.
    GetMyPeerId(oneshot::Sender<PeerId>),

    /// List the files on the local system. The second arg is the transmission channel to send the file list.
    ListLocalFiles(oneshot::Sender<DynResult<QueryResult>>),

    /// List the files on the given peer. The second arg is the transmission channel to send the file list.  The result
    /// should be a [QueryResult::Files].
    ListPeerFiles(PeerId, oneshot::Sender<DynResult<QueryResult>>),

    /// Check if the given file exists on the given peer. The second arg is the transmission channel to send the result.
    /// The result should contain a [QueryResult::Boolean].
    DoesFileExist(PeerId, String, oneshot::Sender<DynResult<QueryResult>>),

    /// Get the given file from the given peer. The second arg is the transmission channel to send true if successful,
    /// and false otherwise.
    GetFileFromPeer(PeerId, String, oneshot::Sender<bool>),
    Shutdown,
}

#[doc(hidden)]
/// Startup banner for [Repl]
static BANNER: &str = "File Sharing Client";
#[doc(hidden)]
static PROMPT: &str = "fsc> ";
#[doc(hidden)]
static HELP: &str = "help";
#[doc(hidden)]
static QUIT: &str = "quit";
#[doc(hidden)]
static PEERS: &str = "peers";
#[doc(hidden)]
static LIST: &str = "list";
#[doc(hidden)]
static GET: &str = "get";
#[doc(hidden)]
static DIAL: &str = "dial";
#[doc(hidden)]
static WHOAMI: &str = "whoami";

/// The behavior of the [Repl] after processing a command.
enum ReplBehavior {
    /// The [Repl] should display the prompt.
    Prompt,

    /// The [Repl] should terminate.
    Quit,
}

/// The [Repl] is the read-eval-print-loop for the client of the file sharing tool.
pub struct Repl {
    /// The channel to send commands to the command core.
    pub command_sender: Sender<ReplCommand>,
}

impl Repl {
    /// Create a new [Repl] with the given command channel.
    ///
    /// # Arguments
    ///
    /// * `command_sender` - The channel to send commands to the command core.
    pub fn new(command_sender: Sender<ReplCommand>) -> Repl {
        Repl { command_sender }
    }

    /// Run the [Repl] loop.
    pub async fn run(&mut self) -> DynResult<()> {
        println!("{}", BANNER);
        println!("Type \"{}\" for help for more information.", HELP);
        write_prompt()?;

        let mut stdin = BufReader::new(tokio::io::stdin()).lines();

        loop {
            while let Some(line) = stdin.next_line().await? {
                let repl_command = self.handle_stdin_input(&line).await?;
                match repl_command {
                    ReplBehavior::Prompt => write_prompt()?,
                    ReplBehavior::Quit => return Ok(()),
                }
            }
        }
    }

    /// Handle the input from the user.
    ///
    /// # Arguments
    ///
    /// * `input_line` - The input from the user.
    async fn handle_stdin_input(&mut self, input_line: &str) -> DynResult<ReplBehavior> {
        let input = input_line.trim().split(" ").collect::<Vec<&str>>();
        if input.is_empty() {
            return Ok(ReplBehavior::Prompt);
        }
        return match input[0] {
            "help" => {
                println!("{} - this help message", HELP);
                println!("{} - display my peer id", WHOAMI);
                println!("{} - quit the application", QUIT);
                println!("{} - list peers", PEERS);
                println!("{} - dial <peer_id> - dial a peer", DIAL);
                println!("{} - list local files", LIST);
                println!("{} <peer_id> - list files for peer", LIST);
                println!("{} <peer_id> <file_name> - get file from peer", GET);
                Ok(ReplBehavior::Prompt)
            }
            "quit" | "exit" => {
                println!("Quitting...");
                self.command_sender.send(ReplCommand::Shutdown).await?;
                Ok(ReplBehavior::Quit)
            }
            "peers" => {
                let (tx, rx) = oneshot::channel::<Vec<PeerId>>();
                self.command_sender
                    .send(ReplCommand::GetPeerList(tx))
                    .await?;
                for peer in rx.await? {
                    println!("{}", peer);
                }
                Ok(ReplBehavior::Prompt)
            }
            "dial" => {
                if input.len() != 2 {
                    println!("{} <peer_id> - dial a peer", DIAL);
                    Ok(ReplBehavior::Prompt)
                } else {
                    let (tx, rx) = oneshot::channel::<DynResult<()>>();
                    if let Ok(peer_id) = PeerId::from_str(input[1]) {
                        self.command_sender
                            .send(ReplCommand::Dial(peer_id, tx))
                            .await?;
                        if !rx.await?.is_ok() {
                            println!("Dial failed");
                        }
                    } else {
                        println!("Invalid peer id: {}", input[1]);
                    }
                    Ok(ReplBehavior::Prompt)
                }
            }
            "list" => {
                let (tx, rx) = oneshot::channel::<DynResult<QueryResult>>();
                if input.len() == 1 {
                    // List the local files on the system.
                    self.command_sender
                        .send(ReplCommand::ListLocalFiles(tx))
                        .await?;
                } else {
                    if let Ok(peer_id) = PeerId::from_str(input[1]) {
                        self.command_sender
                            .send(ReplCommand::ListPeerFiles(peer_id, tx))
                            .await?;
                    } else {
                        println!("Invalid peer id: {}", input[1]);
                        return Ok(ReplBehavior::Prompt);
                    }
                }
                match rx.await? {
                    Ok(result) => match result {
                        QueryResult::Files(files) => {
                            for file in files {
                                println!("{}", file);
                            }
                        }
                        _ => {
                            println!("No files found");
                        }
                    },
                    Err(e) => {
                        println!("Error listing files: {}", e);
                    }
                }
                Ok(ReplBehavior::Prompt)
            }
            "get" => {
                if input.len() != 3 {
                    println!("{} <peer_id> <file_name> - get file from peer", GET);
                    Ok(ReplBehavior::Prompt)
                } else {
                    // First see if the peer id is valid
                    let peer_id = if let Ok(peer_id) = PeerId::from_str(input[1]) {
                        peer_id
                    } else {
                        println!("Invalid peer id: {}", input[1]);
                        return Ok(ReplBehavior::Prompt);
                    };

                    // See if the peer is a valid peer.
                    let (tx, rx) = oneshot::channel::<Vec<PeerId>>();
                    self.command_sender
                        .send(ReplCommand::GetPeerList(tx))
                        .await?;
                    let peer_list = rx.await?;
                    let matched_peers = peer_list
                        .iter()
                        .filter(|p| **p == peer_id)
                        .collect::<Vec<&PeerId>>();

                    if matched_peers.is_empty() {
                        println!("Peer not found");
                        return Ok(ReplBehavior::Prompt);
                    }

                    // Now see if the file exists on the peer.
                    let (tx, rx) = oneshot::channel::<DynResult<QueryResult>>();
                    self.command_sender
                        .send(ReplCommand::DoesFileExist(
                            peer_id,
                            input[2].to_string(),
                            tx,
                        ))
                        .await?;

                    match rx.await {
                        Ok(result) => match result? {
                            QueryResult::Boolean(false) => {
                                println!("File does not exist");
                                return Ok(ReplBehavior::Prompt);
                            }
                            QueryResult::Files(_) => {
                                error!("Received file list result when expecting boolean");
                            }
                            _ => {
                                error!("I'm not sure what happened");
                                return Ok(ReplBehavior::Prompt);
                            }
                        },
                        Err(e) => {
                            println!("Unable to determine if file exists: {}", e);
                            return Ok(ReplBehavior::Prompt);
                        }
                    }

                    // The file exists, now request it.
                    let (tx, rx) = oneshot::channel::<bool>();
                    self.command_sender
                        .send(ReplCommand::GetFileFromPeer(
                            peer_id,
                            input[2].to_string(),
                            tx,
                        ))
                        .await?;
                    let success = rx.await?;
                    if success {
                        println!("File downloaded successfully");
                    } else {
                        println!("File download failed");
                    }
                    Ok(ReplBehavior::Prompt)
                }
            }
            "whoami" => {
                let (tx, rx) = oneshot::channel::<PeerId>();
                self.command_sender
                    .send(ReplCommand::GetMyPeerId(tx))
                    .await?;
                let peer_id = rx.await?;
                println!("{}", peer_id);
                Ok(ReplBehavior::Prompt)
            }
            "" => Ok(ReplBehavior::Prompt),
            _ => {
                println!("Unknown command: {}", input[0]);
                Ok(ReplBehavior::Prompt)
            }
        };
    }
}

/// Write the prompt to stdout.
fn write_prompt() -> DynResult<()> {
    print!("{}", PROMPT);
    stdout().flush()?;
    Ok(())
}
