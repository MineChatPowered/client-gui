use iced::{
    Alignment, Element, Length, Subscription, Task,
    advanced::subscription,
    alignment::{self, Horizontal},
    futures::{self, StreamExt},
    widget::{Button, Column, Container, Row, Scrollable, Text, TextInput},
};
use minechat_protocol::{
    packets::send_message,
    protocol::{
        AuthAckPayload, AuthPayload, BroadcastPayload, ChatPayload, DisconnectPayload,
        MineChatMessage,
    },
};
use std::{hash::Hash, net::SocketAddr};
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
    Received(MineChatMessage),
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

#[derive(Debug, Default)]
struct ChatApp {
    server: String,
    link_code: String,
    mode: Mode,
}

#[derive(Debug, Clone)]
struct ChatConnection {
    outgoing: mpsc::UnboundedSender<String>,
    incoming: broadcast::Sender<MineChatMessage>,
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

    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel();
    let (incoming_tx, _) = broadcast::channel(100);

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
                let _ = incoming_tx_clone.send(msg);
            }
        }
    });

    Ok(ChatConnection {
        outgoing: outgoing_tx,
        incoming: incoming_tx,
    })
}

// --- Subscription Handling ---

struct ChatReceiver {
    id: u32,
    stream: BroadcastStream<MineChatMessage>,
}

impl std::hash::Hash for ChatReceiver {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl subscription::Recipe for ChatReceiver {
    type Output = Message;

    fn hash(&self, state: &mut std::hash::Hasher) {
        self.id.hash(state);
    }

    fn stream(self: Box<Self>) -> futures::stream::BoxStream<'static, Self::Output> {
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
                    match msg {
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
                let title = Text::new("Connect to MineChat Server")
                    .size(30)
                    .horizontal_alignment(alignment::Horizontal::Center);

                let server_input = TextInput::new("Server address", &self.server)
                    .on_input(Message::ServerChanged)
                    .padding(10);

                let link_input = TextInput::new("Link code", &self.link_code)
                    .on_input(Message::LinkCodeChanged)
                    .padding(10);

                let connect_button = Button::new(Text::new("Connect"))
                    .on_press(Message::Connect)
                    .padding(10);

                let content = Column::new()
                    .padding(20)
                    .spacing(20)
                    .align_items(Alignment::Center)
                    .push(title)
                    .push(server_input)
                    .push(link_input)
                    .push(connect_button);

                Container::new(content)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x()
                    .center_y()
                    .into()
            }
            Mode::Connected {
                messages,
                input,
                connection: _,
            } => {
                let messages: Element<_> =
                    Scrollable::new(messages.iter().fold(Column::new().spacing(5), |col, msg| {
                        col.push(Text::new(msg))
                    }))
                    .width(Length::Fill)
                    .height(Length::FillPortion(5))
                    .into();

                let chat_input = TextInput::new("Type a message", input)
                    .on_input(Message::InputChanged)
                    .padding(10)
                    .width(Length::FillPortion(5));

                let send_button = Button::new(Text::new("Send"))
                    .on_press(Message::Send)
                    .padding(10);

                let input_row = Row::new().spacing(10).push(chat_input).push(send_button);

                let content = Column::new()
                    .padding(10)
                    .spacing(10)
                    .push(messages)
                    .push(input_row);

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
                Subscription::from_recipe(ChatReceiver { id: 42, stream })
            }
            _ => Subscription::none(),
        }
    }
}

fn main() -> iced::Result {
    iced::run("", ChatApp::update, ChatApp::view)
}
