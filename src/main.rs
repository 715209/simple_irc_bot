use twitch_irc::twitch;

#[tokio::main]
async fn main() {
    let client = twitch::Client::new("715209", "oauth:xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
    client.connect();
    client.reconnect(false).await;

    let reader = reader(client.messages.clone());
    client.join("715209").await;

    let _ = reader.await;
    println!("Twitch client has stopped.");
}

fn reader(reader: async_channel::Receiver<irc_parser::Message>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(msg) = reader.recv().await {
            //println!("Got message: {:?}", msg);
            //println!();
            #[allow(clippy::single_match)]
            match msg.command.unwrap().as_ref() {
                "PRIVMSG" => {
                    let params = msg.params.unwrap();

                    println!(
                        "[{}] {}: {}\n",
                        params[0],
                        {
                            match msg.prefix.unwrap() {
                                twitch::Prefix::Nick(nick, _, _) => nick,
                                _ => unreachable!(),
                            }
                        },
                        params[1]
                    )
                }
                _ => (),
            }
        }
    })
}
