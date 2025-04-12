use iced::{
    Element, Length, Subscription, Task,
    advanced::subscription,
    alignment::{Horizontal, Vertical},
    futures::{self, Stream, StreamExt},
    widget::{Container, Scrollable, button, column, row, text, text_input},
};
use minechat_protocol::{
    packets::send_message,
    protocol::{
        AuthAckPayload, AuthPayload, BroadcastPayload, ChatPayload, DisconnectPayload,
        MineChatMessage,
    },
};
use std::{hash::Hash, net::SocketAddr, pin::Pin, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpStream,
    sync::{broadcast, mpsc},
};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

#[derive(Debug, Clone)]
enum Message {
    ServerChanged(String),
    LinkCodeChanged(String),
    Connect,
    ConnectionResult(Result<ChatConnection, String>),
    InputChanged(String),
    Send,
    Received(Arc<MineChatMessage>),
    Disconnected(String),
}

#[derive(Debug)]
enum Mode {
    Disconnected,
    Connecting,
    Connected {
        connection: ChatConnection,
        messages: Vec<String>,
        input: String,
    },
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Disconnected
    }
}

#[derive(Debug, Default)]
struct ChatApp {
    server: String,
    link_code: String,
    mode: Mode,
}

#[derive(Debug, Clone)]
struct ChatConnection {
    outgoing: mpsc::UnboundedSender<String>,
    incoming: broadcast::Sender<Arc<MineChatMessage>>,
}

async fn connect_async(server: String, link_code: String) -> Result<ChatConnection, String> {
    let addr: SocketAddr = server
        .parse()
        .map_err(|e| format!("Invalid address: {}", e))?;
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("Failed to connect: {}", e))?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = writer;

    let client_uuid = Uuid::new_v4().to_string();
    let auth_msg = MineChatMessage::Auth {
        payload: AuthPayload {
            client_uuid: client_uuid.clone(),
            link_code,
        },
    };
    send_message(&mut writer, &auth_msg)
        .await
        .map_err(|e| format!("Failed to send auth message: {}", e))?;

    let mut auth_response = String::new();
    reader
        .read_line(&mut auth_response)
        .await
        .map_err(|e| format!("Failed to read auth response: {}", e))?;
    let response: MineChatMessage = serde_json::from_str(&auth_response)
        .map_err(|e| format!("Failed to parse auth response: {}", e))?;

    if let MineChatMessage::AuthAck { payload } = response {
        if payload.status != "success" {
            return Err(format!("Auth failed: {}", payload.message));
        }
    } else {
        return Err("Unexpected response during authentication".into());
    }

    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<String>();
    let (incoming_tx, _) = broadcast::channel::<Arc<MineChatMessage>>(100);

    let mut writer_clone = writer;
    tokio::spawn(async move {
        while let Some(line) = outgoing_rx.recv().await {
            let msg = if line.trim() == "/quit" || line.trim() == "/exit" {
                MineChatMessage::Disconnect {
                    payload: DisconnectPayload {
                        reason: "Client exit".into(),
                    },
                }
            } else {
                MineChatMessage::Chat {
                    payload: ChatPayload { message: line },
                }
            };
            if let Err(e) = send_message(&mut writer_clone, &msg).await {
                eprintln!("Failed to send message: {}", e);
                break;
            }
        }
    });

    let incoming_tx_clone = incoming_tx.clone();
    tokio::spawn(async move {
        let mut reader_lines = reader.lines();
        while let Ok(Some(line)) = reader_lines.next_line().await {
            if let Ok(msg) = serde_json::from_str::<MineChatMessage>(&line) {
                let _ = incoming_tx_clone.send(Arc::new(msg));
            }
        }
    });

    Ok(ChatConnection {
        outgoing: outgoing_tx,
        incoming: incoming_tx,
    })
}

struct ChatReceiver {
    id: u32,
    stream: BroadcastStream<Arc<MineChatMessage>>,
}

impl std::hash::Hash for ChatReceiver {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl subscription::Recipe for ChatReceiver {
    type Output = Message;

    fn hash(&self, state: &mut iced::advanced::subscription::Hasher) {
        self.id.hash(state);
    }

    fn stream(
        self: Box<Self>,
        _input: Pin<Box<dyn Stream<Item = subscription::Event> + Send>>,
    ) -> futures::stream::BoxStream<'static, Self::Output> {
        self.stream
            .filter_map(|result| async move { result.ok().map(|msg| Message::Received(msg)) })
            .boxed()
    }
}

impl ChatApp {
    fn update(&mut self, message: Message) -> Task<Message> {
        match &mut self.mode {
            Mode::Disconnected => match message {
                Message::ServerChanged(val) => {
                    self.server = val;
                    Task::none()
                }
                Message::LinkCodeChanged(val) => {
                    self.link_code = val;
                    Task::none()
                }
                Message::Connect => {
                    self.mode = Mode::Connecting;
                    let server = self.server.clone();
                    let link = self.link_code.clone();
                    Task::perform(connect_async(server, link), Message::ConnectionResult)
                }
                Message::ConnectionResult(Ok(conn)) => {
                    self.mode = Mode::Connected {
                        connection: conn,
                        messages: Vec::new(),
                        input: String::new(),
                    };
                    Task::none()
                }
                Message::ConnectionResult(Err(e)) => {
                    self.mode = Mode::Disconnected;
                    Task::none()
                }
                _ => Task::none(),
            },
            Mode::Connecting => match message {
                Message::ConnectionResult(Ok(conn)) => {
                    self.mode = Mode::Connected {
                        connection: conn,
                        messages: Vec::new(),
                        input: String::new(),
                    };
                    Task::none()
                }
                Message::ConnectionResult(Err(e)) => {
                    self.mode = Mode::Disconnected;
                    Task::none()
                }
                _ => Task::none(),
            },
            Mode::Connected {
                connection,
                messages,
                input,
            } => match message {
                Message::InputChanged(val) => {
                    *input = val;
                    Task::none()
                }
                Message::Send => {
                    let msg = input.trim().to_string();
                    if !msg.is_empty() {
                        let _ = connection.outgoing.send(msg);
                    }
                    *input = String::new();
                    Task::none()
                }
                Message::Received(msg) => {
                    match &*msg {
                        MineChatMessage::Broadcast { payload } => {
                            messages.push(format!("{}: {}", payload.from, payload.message));
                        }
                        MineChatMessage::Chat { payload } => {
                            messages.push(format!("You: {}", payload.message));
                        }
                        MineChatMessage::Disconnect { payload } => {
                            messages.push(format!("Disconnected: {}", payload.reason));
                            self.mode = Mode::Disconnected;
                        }
                        _ => (),
                    }
                    Task::none()
                }
                _ => Task::none(),
            },
        }
    }

    fn view(&self) -> Element<Message> {
        match &self.mode {
            Mode::Disconnected | Mode::Connecting => {
                let title = text("Connect to MineChat Server")
                    .size(30)
                    .align_x(Horizontal::Center)
                    .align_y(Vertical::Center);

                let server_input = text_input("Server address", &self.server)
                    .on_input(Message::ServerChanged)
                    .padding(10);

                let link_input = text_input("Link code", &self.link_code)
                    .on_input(Message::LinkCodeChanged)
                    .padding(10);

                let connect_button = button(text("Connect"))
                    .on_press(Message::Connect)
                    .padding(10);

                let content = column![title, server_input, link_input, connect_button]
                    .padding(20)
                    .spacing(20)
                    .align_x(Horizontal::Center);

                Container::new(content)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center(Length::Fill)
                    .into()
            }
            Mode::Connected {
                messages,
                input,
                connection: _,
            } => {
                let messages: Element<_> = Scrollable::new(
                    messages
                        .iter()
                        .fold(column![].spacing(5), |col, msg| col.push(text(msg))),
                )
                .width(Length::Fill)
                .height(Length::FillPortion(5))
                .into();

                let chat_input = text_input("Type a message", input)
                    .on_input(Message::InputChanged)
                    .padding(10)
                    .width(Length::FillPortion(5));

                let send_button = button(text("Send")).on_press(Message::Send).padding(10);

                let input_row = row![chat_input, send_button].spacing(10);

                let content = column![messages, input_row].padding(10).spacing(10);

                Container::new(content)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        match &self.mode {
            Mode::Connected { connection, .. } => {
                let rx = connection.incoming.subscribe();
                let stream = BroadcastStream::new(rx);
                subscription::from_recipe(ChatReceiver { id: 42, stream })
            }
            _ => Subscription::none(),
        }
    }
}

fn main() -> iced::Result {
    env_logger::init();
    let theme = |_s: &ChatApp| iced::Theme::Dark;
    iced::application("MineChat GUI", ChatApp::update, ChatApp::view)
        .theme(theme)
        .centered()
        .run()
}
