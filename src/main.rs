use iced::{
    Element, Subscription, Task,
    widget::{Column, Scrollable, button, column, row, text, text_input},
};
use log::error;
use minechat_protocol::{
    MessageStream, TokioMessageStream,
    protocol::{AuthPayload, ChatPayload, MineChatMessage},
};
use std::sync::Arc;
use tokio::{
    net::TcpStream,
    sync::{broadcast, mpsc},
};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use uuid::Uuid;

use kyori_component_json::Component;

fn component_to_plain_text(component: &Component) -> String {
    match component {
        Component::String(s) => s.clone(),
        Component::Array(components) => components.iter().map(component_to_plain_text).collect(),
        Component::Object(obj) => {
            let mut text = String::new();
            if let Some(s) = &obj.text {
                text.push_str(s);
            }
            if let Some(extra) = &obj.extra {
                for component in extra {
                    text.push_str(&component_to_plain_text(component));
                }
            }
            text
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    ServerChanged(String),
    LinkCodeChanged(String),
    Connect,
    ConnectionResult(Result<ChatConnection, String>),
    InputChanged(String),
    Send,
    Received(Arc<MineChatMessage>),
}

#[derive(Debug)]
enum State {
    Disconnected {
        server: String,
        link_code: String,
    },
    Connecting,
    Connected {
        connection: ChatConnection,
        messages: Vec<String>,
        input: String,
    },
}

#[derive(Debug, Clone)]
struct ChatConnection {
    outgoing: mpsc::UnboundedSender<MineChatMessage>,
    incoming: broadcast::Sender<Arc<MineChatMessage>>,
}

pub struct ChatApp {
    state: State,
}

impl Default for ChatApp {
    fn default() -> Self {
        Self {
            state: State::Disconnected {
                server: "127.0.0.1:25575".to_string(),
                link_code: "".to_string(),
            },
        }
    }
}

async fn connect(server: String, link_code: String) -> Result<ChatConnection, String> {
    let stream = TcpStream::connect(&server)
        .await
        .map_err(|e| e.to_string())?;
    let mut message_stream = TokioMessageStream::new(stream);

    let auth_payload = AuthPayload {
        client_uuid: Uuid::new_v4().to_string(),
        link_code,
    };

    message_stream
        .send_message(&MineChatMessage::Auth {
            payload: auth_payload,
        })
        .await
        .map_err(|e| e.to_string())?;

    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel();
    let (incoming_tx, _) = broadcast::channel(100);
    let incoming_tx_clone = incoming_tx.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(msg) = outgoing_rx.recv() => {
                    if let Err(e) = message_stream.send_message(&msg).await {
                        error!("Failed to send message: {}", e);
                        break;
                    }
                }
                result = message_stream.receive_message() => {
                    match result {
                        Ok(msg) => {
                            if let Err(e) = incoming_tx_clone.send(Arc::new(msg)) {
                                error!("Failed to forward incoming message: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("Failed to receive message: {}", e);
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok(ChatConnection {
        outgoing: outgoing_tx,
        incoming: incoming_tx,
    })
}

impl ChatApp {
    fn update(&mut self, message: Message) -> Task<Message> {
        let mut new_state = None;
        let mut task = Task::none();

        match &mut self.state {
            State::Disconnected { server, link_code } => match message {
                Message::ServerChanged(s) => {
                    *server = s;
                }
                Message::LinkCodeChanged(l) => {
                    *link_code = l;
                }
                Message::Connect => {
                    new_state = Some(State::Connecting);
                    let server = server.clone();
                    let link_code = link_code.clone();
                    task = Task::perform(connect(server, link_code), Message::ConnectionResult);
                }
                _ => {}
            },
            State::Connecting => match message {
                Message::ConnectionResult(Ok(connection)) => {
                    new_state = Some(State::Connected {
                        connection,
                        messages: vec!["Connected!".to_string()],
                        input: "".to_string(),
                    });
                }
                Message::ConnectionResult(Err(e)) => {
                    error!("Failed to connect: {}", e);
                    new_state = Some(State::Disconnected {
                        server: "127.0.0.1:25575".to_string(),
                        link_code: "".to_string(),
                    });
                }
                _ => {}
            },
            State::Connected {
                connection,
                messages,
                input,
            } => match message {
                Message::InputChanged(i) => {
                    *input = i;
                }
                Message::Send => {
                    let msg = input.trim().to_string();
                    if !msg.is_empty() {
                        let chat_message = MineChatMessage::Chat {
                            payload: ChatPayload {
                                message: Component::text(msg.clone()),
                            },
                        };
                        if let Err(e) = connection.outgoing.send(chat_message) {
                            error!("Failed to send message: {}", e);
                        }
                        messages.push(format!("You: {}", msg));
                    }
                    *input = String::new();
                }
                Message::Received(msg) => match &*msg {
                    MineChatMessage::Broadcast { payload } => {
                        let text = component_to_plain_text(&payload.message);
                        messages.push(format!("[{}] {}", payload.from, text));
                    }
                    MineChatMessage::Disconnect { payload } => {
                        messages.push(format!("Disconnected: {}", payload.reason));
                        new_state = Some(State::Disconnected {
                            server: "127.0.0.1:25575".to_string(),
                            link_code: "".to_string(),
                        });
                    }
                    _ => {}
                },
                _ => {}
            },
        }

        if let Some(new_state) = new_state {
            self.state = new_state;
        }

        task
    }

    fn subscription(&self) -> Subscription<Message> {
        match &self.state {
            State::Connected { connection, .. } => {
                let receiver = connection.incoming.subscribe();
                Subscription::run_with_id(
                    "messages",
                    BroadcastStream::new(receiver)
                        .map(|result| result.unwrap())
                        .map(Message::Received),
                )
            }
            _ => Subscription::none(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        match &self.state {
            State::Disconnected { server, link_code } => {
                let title = text("Connect to MineChat").size(30);
                let server_input =
                    text_input("Server Address", server).on_input(Message::ServerChanged);
                let link_code_input =
                    text_input("Link Code", link_code).on_input(Message::LinkCodeChanged);
                let connect_button = button("Connect").on_press(Message::Connect);

                column![title, server_input, link_code_input, connect_button]
                    .spacing(10)
                    .padding(20)
                    .into()
            }
            State::Connecting => {
                let title = text("Connecting...").size(30);
                column![title].spacing(10).padding(20).into()
            }
            State::Connected {
                messages, input, ..
            } => {
                let messages_col = messages
                    .iter()
                    .fold(Column::new(), |col, msg| col.push(text(msg)));
                let scrollable = Scrollable::new(messages_col);

                let input = text_input("Type a message...", input)
                    .on_input(Message::InputChanged)
                    .on_submit(Message::Send);
                let send_button = button("Send").on_press(Message::Send);

                column![scrollable, row![input, send_button].spacing(10)]
                    .spacing(10)
                    .padding(20)
                    .into()
            }
        }
    }
}

fn main() -> iced::Result {
    env_logger::init();
    iced::application("MineChat GUI", ChatApp::update, ChatApp::view)
        .subscription(ChatApp::subscription)
        .run_with(|| (ChatApp::default(), Task::none()))
}


