use futures::{join, select, StreamExt};
use irc_parser::Message;
use native_tls::TlsConnector;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time;
//use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio_native_tls::TlsStream;
use tokio_util::codec::{FramedRead, LinesCodec};

pub use irc_parser::Prefix;

#[derive(Clone)]
pub struct Client {
    username: String,
    oauth: String,
    pub messages: async_channel::Receiver<Message>,
    messages_tx: async_channel::Sender<Message>,
    internal_tx: async_channel::Sender<String>,
    internal_rx: async_channel::Receiver<String>,
    pub capabilities: Vec<String>,
    channels: Arc<Mutex<Vec<String>>>,
    reconnect: Arc<Mutex<Reconnect>>,
}

#[derive(Clone)]
struct Reconnect {
    should_reconnect: bool,
    attempt: u32,
    last_attempt: u64,
}

impl Client {
    pub fn new(username: &str, oauth: &str) -> Client {
        // Channel to send messages back.
        let (messages_tx, messages) = async_channel::unbounded();

        // Used to send back to twitch
        let (internal_tx, internal_rx) = async_channel::unbounded();

        Client {
            username: username.to_owned(),
            oauth: oauth.to_owned(),
            messages,
            messages_tx,
            internal_tx,
            internal_rx,
            capabilities: vec!["twitch.tv/tags".to_owned(), "twitch.tv/commands".to_owned()],
            channels: Arc::new(Mutex::new(Vec::new())),
            reconnect: Arc::new(Mutex::new(Reconnect {
                should_reconnect: true,
                attempt: 0,
                last_attempt: 0,
            })),
        }
    }

    /// Set if the client should reconnect.
    pub async fn reconnect(&self, reconnect: bool) {
        self.reconnect.lock().await.should_reconnect = reconnect;
    }

    /// Set the capabilities to use.
    async fn caps(&self) {
        todo!();
    }

    /// Connect to the twitch IRC server.
    pub fn connect(&self) {
        // TODO: Can this be done differently?
        let my_own_self = self.clone();

        // TODO: How to handle errors.
        tokio::spawn(async { Client::run(my_own_self).await });
    }

    async fn run(self) {
        loop {
            let send_msg = self.messages_tx.clone();
            let rx_internal = self.internal_rx.clone();

            let addr = "irc.chat.twitch.tv:6697"
                .to_socket_addrs()
                .unwrap()
                .next()
                .unwrap();
            //eprintln!("Failed to resolve irc.chat.twitch.tv");

            // Create socket and connect
            let socket = TcpStream::connect(&addr).await.unwrap();
            let cx = TlsConnector::builder().build().unwrap();
            let cx = tokio_native_tls::TlsConnector::from(cx);
            let socket = cx.connect("irc.chat.twitch.tv", socket).await.unwrap();
            let (rd, mut wr) = tokio::io::split(socket);

            if !self.capabilities.is_empty() {
                let _ = wr
                    .write_all(format!("CAP REQ :{}\r\n", self.capabilities.join(" ")).as_bytes())
                    .await;
            }

            let _ = wr
                .write_all(format!("PASS {}\r\n", &self.oauth).as_bytes())
                .await;
            let _ = wr
                .write_all(format!("NICK {}\r\n", &self.username).as_bytes())
                .await;

            let reconnect_attempts = { self.reconnect.lock().await.attempt };

            if reconnect_attempts != 0 {
                for channel in { self.channels.lock().await }.iter() {
                    self.send_message(&format!("JOIN #{}", channel)).await;
                }
            }

            // Used to shutdown the writer
            let (shutdown_sender, shutdown_receiver) = async_channel::unbounded();

            // Reader
            let r_rec = self.reconnect.clone();
            let tx_writer = self.internal_tx.clone();
            let rj = tokio::spawn(async move {
                Self::handle_read(rd, send_msg, shutdown_sender, r_rec, tx_writer).await
            });

            // Writer
            let shutdown_w = shutdown_receiver.clone();
            let wj =
                tokio::spawn(async move { Self::handle_write(wr, rx_internal, shutdown_w).await });

            // Wait for both tasks to finish
            let _ = join!(rj, wj);

            println!("Disconnected from twitch :(");

            if !{ self.reconnect.lock().await.should_reconnect } {
                break;
            }

            // TODO: Add time to compare because if the last attempt was a while ago this should
            // reset the attempts
            println!("Reconnecting in {} seconds", 1 << reconnect_attempts);
            tokio::time::delay_for(std::time::Duration::from_secs(1 << reconnect_attempts)).await;

            self.reconnect.lock().await.attempt += 1;
        }

        // Close the sender since there won't be anything send anymore
        self.messages_tx.close();
    }

    // TODO: Maybe keep the time of the last received message so if there hasn't been a message in
    // a while send a ping of our own.
    async fn handle_read(
        read: tokio::io::ReadHalf<TlsStream<TcpStream>>,
        send_msg: async_channel::Sender<Message>,
        shutdown: async_channel::Sender<()>,
        reconnect: Arc<Mutex<Reconnect>>,
        internal_send: async_channel::Sender<String>,
    ) {
        let mut read = FramedRead::new(read, LinesCodec::new());

        while let Some(result) = read.next().await {
            match result {
                Ok(line) => {
                    if line.contains("Login authentication failed") {
                        println!("Authentication failed");
                        reconnect.lock().await.should_reconnect = false;
                    }

                    let message = Message::parse(&line);

                    match message {
                        Ok(parsed) => {
                            //println!("RAW {}", line);

                            match parsed.command.as_deref() {
                                // Some("PRIVMSG") => {
                                //     if parsed.params.to_owned().unwrap()[1] == "quit" {
                                //         shutdown.close();
                                //         break;
                                //     }
                                // }
                                Some("PING") => {
                                    let _ =
                                        internal_send.send("PONG :tmi.twitch.tv".to_owned()).await;
                                    //println!("Sending PONG!");
                                }
                                _ => (),
                            }

                            let _ = send_msg.send(parsed).await;
                        }
                        Err(e) => println!("Got an error parsing: {}", e),
                    }
                }
                Err(e) => println!("Reading line error: {}", e),
            }
        }

        // TODO: Since shutdown sender is dropped, it automatically shutsdown the writer and
        // keepalive so do i need this?
        shutdown.close();
    }

    // TODO: rate limiting sending messages
    async fn handle_write(
        mut writer: tokio::io::WriteHalf<TlsStream<TcpStream>>,
        message_queue: async_channel::Receiver<String>,
        shutdown: async_channel::Receiver<()>,
    ) {
        let mut message = message_queue.fuse();
        let mut shutdown = shutdown.fuse();

        loop {
            select! {
                msg = message.next() => match msg {
                    Some(mut message) => {
                        message += "\r\n";
                        let t = writer.write(message.as_bytes()).await;
                        //println!("Wrote bytes: {:?}", t);
                    },
                    None => break,
                },
                sd = shutdown.next() => match sd {
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }

    pub async fn send_message(&self, message: &str) {
        if let Err(error) = self.internal_tx.send(message.to_owned()).await {
            println!("Got err sending a message: {}", error);
        }
    }

    pub async fn join(&self, channel: &str) {
        {
            self.channels
                .lock()
                .await
                .push(channel.to_owned().to_lowercase());
        }

        self.send_message(&format!("JOIN #{}", channel.to_lowercase()))
            .await;
    }
}
