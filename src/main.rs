use iced::futures;
use iced::futures::StreamExt;
use iced::{
    Alignment, Application, Button, Column, Command, Container, Element, Length, Row, Scrollable,
    Settings, Subscription, Text, TextInput, button, executor, scrollable, text_input,
};
use minechat_protocol::{
    packets::{self, send_message},
    protocol::{
        AuthAckPayload, AuthPayload, BroadcastPayload, ChatPayload, DisconnectPayload,
        MineChatError, MineChatMessage,
    },
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

// --- Application Messages ---

#[derive(Debug, Clone)]
enum AppMessage {
    // Login view messages
    ServerChanged(String),
    LinkCodeChanged(String),
    Connect, // user pressed Connect button
    ConnectionResult(Result<ChatConnection, String>),

    // Chat view messages
    InputChanged(String),
    Send,
    Received(MineChatMessage),
    Disconnected(String),
}

// --- Application State ---

#[derive(Debug)]
enum Mode {
    Disconnected,
    Connecting,
    Connected {
        connection: ChatConnection,
        // messages received (displayed in the chat view)
        messages: Vec<String>,
        // current user input
        input: String,
    },
}

struct ChatApp {
    // UI state for the login view:
    server: String,
    link_code: String,
    connect_button: button::State,
    // UI state for the chat view:
    chat_send_button: button::State,
    chat_input_state: text_input::State,
    scroll: scrollable::State,
    // current mode of the app
    mode: Mode,
}

impl Default for ChatApp {
    fn default() -> Self {
        Self {
            server: String::new(),
            link_code: String::new(),
            connect_button: button::State::new(),
            chat_send_button: button::State::new(),
            chat_input_state: text_input::State::new(),
            scroll: scrollable::State::new(),
            mode: Mode::Disconnected,
        }
    }
}

// --- ChatConnection: holds channels for outgoing and incoming messages ---

#[derive(Clone)]
struct ChatConnection {
    /// Sender for messages typed by the user
    outgoing: mpsc::UnboundedSender<String>,
    /// Broadcast channel used for network messages (incoming messages)
    incoming: broadcast::Sender<MineChatMessage>,
}

// --- Implementation of async connection establishment ---
// This function uses the underlying protocol to connect, authenticate,
// and then spawn two background tasks: one to write out messages from the UI
// and one to read messages from the server.
async fn connect_async(server: String, link_code: String) -> Result<ChatConnection, String> {
    // Attempt to parse and connect to the given address.
    let addr: SocketAddr = server
        .parse()
        .map_err(|e| format!("Invalid address: {}", e))?;
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("Failed to connect: {}", e))?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = writer;

    // For authentication we need to get a client UUID.
    // (Here, for simplicity, we always generate a new one.)
    let client_uuid = Uuid::new_v4().to_string();

    // Send an AUTH message (using the underlying protocol function)
    let auth_msg = MineChatMessage::Auth {
        payload: AuthPayload {
            client_uuid: client_uuid.clone(),
            link_code: link_code.clone(), // might be empty
        },
    };
    send_message(&mut writer, &auth_msg)
        .await
        .map_err(|e| format!("Failed to send auth message: {}", e))?;

    // Wait for the AUTH_ACK
    let mut auth_response = String::new();
    reader
        .read_line(&mut auth_response)
        .await
        .map_err(|e| format!("Failed to read auth response: {}", e))?;
    let response: MineChatMessage = serde_json::from_str(&auth_response)
        .map_err(|e| format!("Failed to parse auth response: {}", e))?;
    match response {
        MineChatMessage::AuthAck { payload } => {
            if payload.status != "success" {
                return Err(format!("Auth failed: {}", payload.message));
            }
            // Continue with connection
        }
        _ => return Err("Unexpected response during authentication".into()),
    }

    // Create a channel for outgoing messages (from UI to writer task)
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<String>();

    // Create a broadcast channel for incoming messages (from reader task to UI subscription)
    let (incoming_tx, _incoming_rx) = broadcast::channel(100);

    // Spawn a background task to send user messages to the server.
    // This task will take messages from the `outgoing_rx` channel and send them using the protocol.
    let mut writer_clone = writer;
    tokio::spawn(async move {
        while let Some(line) = outgoing_rx.recv().await {
            // if the user types a disconnect command, send a disconnect message:
            if line.trim() == "/quit" || line.trim() == "/exit" {
                let disconnect_msg = MineChatMessage::Disconnect {
                    payload: DisconnectPayload {
                        reason: "Client exit".into(),
                    },
                };
                let _ = send_message(&mut writer_clone, &disconnect_msg).await;
                break;
            } else {
                // Otherwise, send a chat message:
                let chat_msg = MineChatMessage::Chat {
                    payload: ChatPayload { message: line },
                };
                if let Err(e) = send_message(&mut writer_clone, &chat_msg).await {
                    eprintln!("Failed to send chat message: {}", e);
                    break;
                }
            }
        }
    });

    // Spawn a background task to read messages from the server.
    // For each received message, send it into the broadcast channel.
    let incoming_tx_clone = incoming_tx.clone();
    tokio::spawn(async move {
        let mut reader_lines = reader.lines();
        loop {
            match reader_lines.next_line().await {
                Ok(Some(line)) => match serde_json::from_str::<MineChatMessage>(&line) {
                    Ok(msg) => {
                        let _ = incoming_tx_clone.send(msg);
                    }
                    Err(e) => {
                        eprintln!("Failed to parse incoming message: {}", e);
                    }
                },
                Ok(None) => {
                    // Connection closed
                    break;
                }
                Err(e) => {
                    eprintln!("Error reading from server: {}", e);
                    break;
                }
            }
        }
    });

    Ok(ChatConnection {
        outgoing: outgoing_tx,
        incoming: incoming_tx,
    })
}

// --- Iced Subscription Recipe for Incoming Messages ---

#[derive(Debug, Clone)]
struct ChatReceiver {
    // A unique ID so that Iced knows whether subscriptions are identical
    id: u32,
    // The broadcast receiver wrapped as a stream (from tokio_stream)
    stream: BroadcastStream<MineChatMessage>,
}

impl std::hash::Hash for ChatReceiver {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<H, I> iced_native::subscription::Recipe<H, I> for ChatReceiver
where
    H: std::hash::Hasher,
{
    type Output = AppMessage;

    fn hash(&self, state: &mut H) {
        // Use the unique id to determine uniqueness.
        self.id.hash(state);
    }

    fn stream(self: Box<Self>, _input: I) -> futures::stream::BoxStream<'static, Self::Output> {
        // Convert the broadcast stream into a stream of AppMessage::Received.
        // Note: we ignore errors from BroadcastStream (they occur when lagging behind).
        self.stream
            .filter_map(|result| {
                futures::future::ready(match result {
                    Ok(msg) => Some(AppMessage::Received(msg)),
                    Err(_) => None,
                })
            })
            .boxed()
    }
}

// --- Implementation of the Iced Application ---

impl Application for ChatApp {
    type Executor = executor::Tokio;
    type Message = AppMessage;
    type Flags = ();

    fn new(_flags: ()) -> (Self, Command<Self::Message>) {
        (ChatApp::default(), Command::none())
    }

    fn title(&self) -> String {
        String::from("MineChat (Iced GUI)")
    }

    fn update(&mut self, message: Self::Message) -> Command<Self::Message> {
        match &mut self.mode {
            Mode::Disconnected => match message {
                AppMessage::ServerChanged(val) => {
                    self.server = val;
                    Command::none()
                }
                AppMessage::LinkCodeChanged(val) => {
                    self.link_code = val;
                    Command::none()
                }
                AppMessage::Connect => {
                    // When the user presses "Connect", switch to Connecting state and spawn the async task.
                    self.mode = Mode::Connecting;
                    let server = self.server.clone();
                    let link = self.link_code.clone();
                    return Command::perform(
                        connect_async(server, link),
                        AppMessage::ConnectionResult,
                    );
                }
                AppMessage::ConnectionResult(Ok(connection)) => {
                    // Switch to the Connected state.
                    self.mode = Mode::Connected {
                        connection,
                        messages: Vec::new(),
                        input: String::new(),
                    };
                    Command::none()
                }
                AppMessage::ConnectionResult(Err(err)) => {
                    // If connection fails, log error and go back to Disconnected.
                    eprintln!("Connection error: {}", err);
                    self.mode = Mode::Disconnected;
                    Command::none()
                }
                _ => Command::none(),
            },
            Mode::Connecting => match message {
                AppMessage::ConnectionResult(Ok(connection)) => {
                    self.mode = Mode::Connected {
                        connection,
                        messages: Vec::new(),
                        input: String::new(),
                    };
                    Command::none()
                }
                AppMessage::ConnectionResult(Err(err)) => {
                    eprintln!("Connection error: {}", err);
                    self.mode = Mode::Disconnected;
                    Command::none()
                }
                _ => Command::none(),
            },
            Mode::Connected {
                connection,
                messages,
                input,
            } => match message {
                AppMessage::InputChanged(val) => {
                    *input = val;
                    Command::none()
                }
                AppMessage::Send => {
                    // Send the input text to the server.
                    let msg = input.trim().to_string();
                    if !msg.is_empty() {
                        if let Err(e) = connection.outgoing.send(msg) {
                            eprintln!("Failed to send message: {}", e);
                        }
                    }
                    *input = String::new();
                    Command::none()
                }
                AppMessage::Received(msg) => {
                    // Process the incoming message from the server.
                    match msg {
                        MineChatMessage::Broadcast { payload } => {
                            messages.push(format!("{}: {}", payload.from, payload.message));
                        }
                        MineChatMessage::Chat { payload } => {
                            messages.push(format!("(you) {}", payload.message));
                        }
                        MineChatMessage::Disconnect { payload } => {
                            messages.push(format!("Disconnected: {}", payload.reason));
                            // Move back to Disconnected mode if server disconnects.
                            self.mode = Mode::Disconnected;
                        }
                        _ => {
                            // For other message types, just log them.
                            messages.push(format!("Received: {:?}", msg));
                        }
                    }
                    Command::none()
                }
                AppMessage::Disconnected(reason) => {
                    messages.push(format!("Disconnected: {}", reason));
                    self.mode = Mode::Disconnected;
                    Command::none()
                }
                _ => Command::none(),
            },
        }
    }

    fn view(&mut self) -> Element<Self::Message> {
        match &mut self.mode {
            Mode::Disconnected | Mode::Connecting => {
                // Show login form.
                let title = Text::new("Connect to MineChat Server")
                    .size(40)
                    .horizontal_alignment(iced::HorizontalAlignment::Center);
                let server_input = TextInput::new(
                    &mut self.chat_input_state, // reusing state for simplicity
                    "Server address (e.g. 127.0.0.1:25575)",
                    &self.server,
                    AppMessage::ServerChanged,
                )
                .padding(10)
                .size(20);

                let link_input = TextInput::new(
                    &mut self.chat_input_state, // reusing same state; in a refined app use separate states
                    "Link code (optional)",
                    &self.link_code,
                    AppMessage::LinkCodeChanged,
                )
                .padding(10)
                .size(20);

                let connect_button = Button::new(&mut self.connect_button, Text::new("Connect"))
                    .on_press(AppMessage::Connect)
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
                    .center_y()
                    .center_x()
                    .into()
            }
            Mode::Connected {
                messages, input, ..
            } => {
                // Show chat view.
                let messages: Element<_> = messages
                    .iter()
                    .fold(Column::new().spacing(5), |column, message| {
                        column.push(Text::new(message).size(16))
                    })
                    .into();

                let chat_input = TextInput::new(
                    &mut self.chat_input_state,
                    "Type a message",
                    input,
                    AppMessage::InputChanged,
                )
                .padding(10)
                .size(20);

                let send_button = Button::new(&mut self.chat_send_button, Text::new("Send"))
                    .on_press(AppMessage::Send);

                let input_row = Row::new().spacing(10).push(chat_input).push(send_button);

                let content = Column::new()
                    .padding(20)
                    .spacing(10)
                    .push(Scrollable::new(&mut self.scroll).push(messages))
                    .push(input_row);

                Container::new(content)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
        }
    }

    // Use a subscription to listen for incoming messages from the network connection.
    fn subscription(&self) -> Subscription<Self::Message> {
        match &self.mode {
            // Only subscribe if connected.
            Mode::Connected { connection, .. } => {
                // Create a fresh receiver from the broadcast channel.
                let rx = connection.incoming.subscribe();
                // Wrap it as a tokio_stream::wrappers::BroadcastStream.
                let stream = BroadcastStream::new(rx);
                // Use our custom subscription recipe.
                Subscription::from_recipe(ChatReceiver { id: 42, stream })
            }
            _ => Subscription::none(),
        }
    }
}

#[tokio::main]
async fn main() {
    // Initialize logger if desired
    env_logger::init();

    ChatApp::run(Settings::default()).await.unwrap();
}
