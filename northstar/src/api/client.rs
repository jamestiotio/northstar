// Copyright (c) 2020 ESRLabs
//
//   Licensed under the Apache License, Version 2.0 (the "License");
//   you may not use this file except in compliance with the License.
//   You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//   Unless required by applicable law or agreed to in writing, software
//   distributed under the License is distributed on an "AS IS" BASIS,
//   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//   See the License for the specific language governing permissions and
//   limitations under the License.

use futures::{SinkExt, Stream, StreamExt};
use log::info;
use std::{collections::HashMap, pin::Pin, task::Poll};
use thiserror::Error;
use tokio::{
    io,
    net::TcpStream,
    select,
    sync::{mpsc, oneshot},
    task, time,
};

use crate::runtime::RepositoryId;

use super::{
    codec::framed,
    model::{Container, Message, Notification, Payload, Repository, Request, Response},
};

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error")]
    Io(#[from] io::Error),
    #[error("Timeout")]
    Timeout,
    #[error("Client is stopped")]
    Stopped,
    #[error("Protocol error")]
    Protocol,
    #[error("Pending request")]
    PendingRequest,
    #[error("Api error")]
    Api(super::model::Error),
}

/// Client for a Northstar runtime instance.
///
/// ```no_run
/// use futures::StreamExt;
/// use northstar::api::client::Client;
///
/// #[tokio::main]
/// async fn main() {
///     let mut client = Client::new("localhost:4200").await.unwrap();
///     client.start("hello").await.expect("Failed to start \"hello\"");
///     while let Some(notification) = client.next().await {
///         println!("{:?}", notification);
///     }
/// }
/// ```
pub struct Client {
    notification_rx: mpsc::Receiver<Result<Notification, Error>>,
    request_tx: mpsc::Sender<(Request, oneshot::Sender<Result<Response, Error>>)>,
}

impl Client {
    /// Create a new northstar client and connect to a runtime instance running on `host`.
    pub async fn new(host: &str) -> Result<Client, Error> {
        let host = host.to_string();
        let (notification_tx, notification_rx) = mpsc::channel(10);
        let (request_tx, mut request_rx) =
            mpsc::channel::<(Request, oneshot::Sender<Result<Response, Error>>)>(10);
        let mut response_tx = Option::<oneshot::Sender<Result<Response, Error>>>::None;
        let mut connection =
            match time::timeout(time::Duration::from_secs(2), TcpStream::connect(host)).await {
                Ok(connection) => framed(connection.map_err(Error::Io)?),
                Err(_) => return Err(Error::Timeout),
            };

        task::spawn(async move {
            loop {
                select! {
                    message = connection.next() => {
                        match message {
                            Some(Ok(message)) => match message.payload {
                                Payload::Request(_) => break Err(Error::Protocol),
                                Payload::Response(r) => {
                                    if let Some(r_tx) = response_tx.take() {
                                        r_tx.send(Ok(r)).ok();
                                    } else {
                                        break Err(Error::Protocol);
                                    }
                                }
                                Payload::Notification(n) => drop(notification_tx.send(Ok(n)).await),
                            },
                            Some(Err(e)) => break Err(Error::Io(e)),
                            None => {
                                    info!("Connection closed");
                                    break Ok(());
                            }
                        }
                    }
                    request = request_rx.recv() => {
                        if let Some((request, r_tx)) = request {
                            if response_tx.is_some() {
                                r_tx.send(Err(Error::PendingRequest)).ok();
                            } else {
                                match connection.send(Message::new_request(request)).await {
                                    Ok(_) => response_tx = Some(r_tx), // Store the reponse tx part
                                    Err(e) => drop(r_tx.send(Err(Error::Io(e)))),
                                }
                            }
                        } else {
                            break Ok(());
                        }
                    }
                }
            }
        });

        Ok(Client {
            notification_rx,
            request_tx,
        })
    }

    /// Perform a request reponse sequence
    ///
    /// ```no_run
    /// # use futures::StreamExt;
    /// # use northstar::api::client::Client;
    /// # use northstar::api::model::Request::Containers;
    /// #
    /// # #[tokio::main]
    /// # async fn main() {
    /// #   let mut client = Client::new("localhost:4200").await.unwrap();
    /// let response = client.request(Containers).await.expect("Failed to request container list");
    /// println!("{:?}", response);
    /// # }
    /// ```
    pub async fn request(&self, request: Request) -> Result<Response, Error> {
        let (tx, rx) = oneshot::channel::<Result<Response, Error>>();
        self.request_tx
            .send((request, tx))
            .await
            .map_err(|_| Error::Stopped)?;
        rx.await.map_err(|_| Error::Stopped)?
    }

    /// Request a list of installed containers
    ///
    /// ```no_run
    /// # use futures::StreamExt;
    /// # use northstar::api::client::Client;
    /// #
    /// # #[tokio::main]
    /// # async fn main() {
    /// #   let mut client = Client::new("localhost:4200").await.unwrap();
    /// let containers = client.containers().await.expect("Failed to request container list");
    /// println!("{:#?}", containers);
    /// # }
    /// ```
    pub async fn containers(&self) -> Result<Vec<Container>, Error> {
        match self.request(Request::Containers).await? {
            Response::Ok(()) => Err(Error::Protocol),
            Response::Containers(containers) => Ok(containers),
            Response::Err(e) => Err(Error::Api(e)),
            Response::Repositories(_) => Err(Error::Protocol),
        }
    }

    /// Request a list of repositories
    ///
    /// ```no_run
    /// # use futures::StreamExt;
    /// # use northstar::api::client::Client;
    /// #
    /// # #[tokio::main]
    /// # async fn main() {
    /// #   let mut client = Client::new("localhost:4200").await.unwrap();
    /// let repositories = client.repositories().await.expect("Failed to request repository list");
    /// println!("{:#?}", repositories);
    /// # }
    /// ```
    pub async fn repositories(&self) -> Result<HashMap<RepositoryId, Repository>, Error> {
        match self.request(Request::Repositories).await? {
            Response::Ok(()) => Err(Error::Protocol),
            Response::Containers(_) => Err(Error::Protocol),
            Response::Err(e) => Err(Error::Api(e)),
            Response::Repositories(repositories) => Ok(repositories),
        }
    }

    /// Start container with name
    ///
    /// ```no_run
    /// # use futures::StreamExt;
    /// # use northstar::api::client::Client;
    /// #
    /// # #[tokio::main]
    /// # async fn main() {
    /// #   let mut client = Client::new("localhost:4200").await.unwrap();
    /// client.start("hello").await.expect("Failed to start \"hello\"");
    /// // Print start notification
    /// println!("{:#?}", client.next().await);
    /// # }
    /// ```
    pub async fn start(&self, name: &str) -> Result<(), Error> {
        match self.request(Request::Start(name.to_string())).await? {
            Response::Ok(()) => Ok(()),
            Response::Containers(_) => Err(Error::Protocol),
            Response::Err(e) => Err(Error::Api(e)),
            Response::Repositories(_) => Err(Error::Protocol),
        }
    }

    /// Stop container with name
    ///
    /// ```no_run
    /// # use futures::StreamExt;
    /// # use northstar::api::client::Client;
    /// #
    /// # #[tokio::main]
    /// # async fn main() {
    /// #   let mut client = Client::new("localhost:4200").await.unwrap();
    /// client.stop("hello").await.expect("Failed to stop \"hello\"");
    /// // Print stop notification
    /// println!("{:#?}", client.next().await);
    /// # }
    /// ```
    pub async fn stop(&self, name: &str) -> Result<(), Error> {
        match self.request(Request::Stop(name.to_string())).await? {
            Response::Ok(()) => Ok(()),
            Response::Containers(_) => Err(Error::Protocol),
            Response::Err(e) => Err(Error::Api(e)),
            Response::Repositories(_) => Err(Error::Protocol),
        }
    }
}

/// Stream notifications
///
/// ```no_run
/// use futures::StreamExt;
/// use northstar::api::client::Client;
///
/// #[tokio::main]
/// async fn main() {
///     let mut client = Client::new("localhost:4200").await.unwrap();
///     client.start("hello").await.expect("Failed to start \"hello\"");
///     while let Some(notification) = client.next().await {
///         println!("{:?}", notification);
///     }
/// }
/// ```
impl Stream for Client {
    type Item = Result<Notification, Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.notification_rx).poll_recv(cx)
    }
}
