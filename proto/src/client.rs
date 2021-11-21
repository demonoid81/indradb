use std::convert::TryInto;
use std::error::Error as StdError;
use std::fmt;

use crate::ConversionError;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::codec::Streaming;
use tonic::transport::{Channel, Endpoint, Error as TonicTransportError};
use tonic::{Request, Status};
use uuid::Uuid;

const CHANNEL_CAPACITY: usize = 100;

fn check_request_id(expected: u32, actual: u32) -> Result<(), ClientError> {
    if expected != actual {
        Err(ClientError::UnexpectedResponseId { expected, actual })
    } else {
        Ok(())
    }
}

/// The error returned if a client operation failed.
#[derive(Debug)]
pub enum ClientError {
    /// Conversion between an IndraDB and its protobuf equivalent failed.
    Conversion { inner: ConversionError },
    /// A gRPC stream response had an unexpected response ID, implying a bug.
    UnexpectedResponseId { expected: u32, actual: u32 },
    /// A gRPC stream response had an unexpected empty body, implying a bug.
    UnexpectedEmptyResponse { request_id: u32 },
    /// A gRPC error.
    Grpc { inner: Status },
    /// A transport error.
    Transport { inner: TonicTransportError },
    /// The gRPC channel has been closed.
    ChannelClosed,
}

impl StdError for ClientError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match *self {
            ClientError::Conversion { ref inner } => Some(inner),
            ClientError::Grpc { ref inner } => Some(inner),
            ClientError::Transport { ref inner } => Some(inner),
            _ => None,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ClientError::Conversion { ref inner } => inner.fmt(f),
            ClientError::UnexpectedResponseId { expected, actual } => {
                write!(f, "unexpected response ID; expected {}, got {}", expected, actual)
            }
            ClientError::UnexpectedEmptyResponse { request_id } => {
                write!(f, "unexpected empty response for request ID {}", request_id)
            }
            ClientError::Grpc { ref inner } => write!(f, "grpc error: {}", inner),
            ClientError::Transport { ref inner } => write!(f, "transport error: {}", inner),
            ClientError::ChannelClosed => write!(f, "failed to send request: channel closed"),
        }
    }
}

impl From<ConversionError> for ClientError {
    fn from(err: ConversionError) -> Self {
        ClientError::Conversion { inner: err }
    }
}

impl From<Status> for ClientError {
    fn from(err: Status) -> Self {
        ClientError::Grpc { inner: err }
    }
}

impl From<TonicTransportError> for ClientError {
    fn from(err: TonicTransportError) -> Self {
        ClientError::Transport { inner: err }
    }
}

impl<T> From<mpsc::error::SendError<T>> for ClientError {
    fn from(_: mpsc::error::SendError<T>) -> Self {
        ClientError::ChannelClosed
    }
}

/// A higher-level client implementation.
///
/// This should be better suited than the low-level client auto-generated by
/// gRPC/tonic in virtually every case, unless you want to avoid the cost of
/// translating between protobuf types and their IndraDB equivalents. The
/// interface is designed to resemble the datastore and transaction traits in
/// IndraDB, but they cannot implement them directly since the functions here
/// are async.
#[derive(Clone)]
pub struct Client(crate::ProtoClient<Channel>);

impl Client {
    /// Creates a new client.
    ///
    /// # Arguments
    /// * `endpoint`: The server endpoint.
    pub async fn new(endpoint: Endpoint) -> Result<Self, ClientError> {
        let client = crate::ProtoClient::connect(endpoint).await?;
        Ok(Client { 0: client })
    }

    /// Pings the server.
    pub async fn ping(&mut self) -> Result<(), ClientError> {
        self.0.ping(()).await?;
        Ok(())
    }

    /// Syncs persisted content. Depending on the datastore implementation,
    /// this has different meanings - including potentially being a no-op.
    pub async fn sync(&mut self) -> Result<(), ClientError> {
        self.0.sync(()).await?;
        Ok(())
    }

    /// Bulk inserts many vertices, edges, and/or properties.
    ///
    /// Note that datastores have discretion on how to approach safeguard vs
    /// performance tradeoffs. In particular:
    /// * If the datastore is disk-backed, it may or may not flush before
    ///   returning.
    /// * The datastore might not verify for correctness; e.g., it might not
    ///   ensure that the relevant vertices exist before inserting an edge.
    /// If you want maximum protection, use the equivalent functions in
    /// transactions, which will provide more safeguards.
    ///
    /// # Arguments
    /// * `items`: The items to insert.
    pub async fn bulk_insert<I>(&mut self, items: I) -> Result<(), ClientError>
    where
        I: Iterator<Item = indradb::BulkInsertItem>,
    {
        let items: Vec<indradb::BulkInsertItem> = items.collect();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        tokio::spawn(async move {
            for item in items.into_iter() {
                if tx.send(item.into()).await.is_err() {
                    return;
                }
            }
        });

        self.0.bulk_insert(Request::new(ReceiverStream::new(rx))).await?;
        Ok(())
    }

    /// Creates a new transaction.
    pub async fn transaction(&mut self) -> Result<Transaction, ClientError> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let response = self.0.transaction(Request::new(ReceiverStream::new(rx))).await?;
        Ok(Transaction::new(tx, response.into_inner()))
    }

    pub async fn index_property<T: Into<indradb::Identifier>>(&mut self, name: T) -> Result<(), ClientError> {
        self.0
            .index_property(Request::new(crate::IndexPropertyRequest {
                name: Some(name.into().into()),
            }))
            .await?;
        Ok(())
    }
}

/// A transaction.
pub struct Transaction {
    sender: mpsc::Sender<crate::TransactionRequest>,
    receiver: Streaming<crate::TransactionResponse>,
    next_request_id: u32,
}

impl Transaction {
    fn new(sender: mpsc::Sender<crate::TransactionRequest>, receiver: Streaming<crate::TransactionResponse>) -> Self {
        Transaction {
            sender,
            receiver,
            next_request_id: 0,
        }
    }

    async fn request(&mut self, request: crate::TransactionRequestVariant) -> Result<u32, ClientError> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        self.sender
            .send(crate::TransactionRequest {
                request_id,
                request: Some(request),
            })
            .await?;

        Ok(request_id)
    }

    async fn request_single(
        &mut self,
        request: crate::TransactionRequestVariant,
    ) -> Result<crate::TransactionResponseVariant, ClientError> {
        let expected_request_id = self.request(request).await?;
        match self.receiver.message().await? {
            Some(crate::TransactionResponse {
                request_id,
                response: Some(response),
            }) => {
                check_request_id(expected_request_id, request_id)?;
                Ok(response)
            }
            _ => Err(ClientError::UnexpectedEmptyResponse {
                request_id: expected_request_id,
            }),
        }
    }

    async fn request_multi(
        &mut self,
        request: crate::TransactionRequestVariant,
    ) -> Result<Vec<crate::TransactionResponseVariant>, ClientError> {
        let expected_request_id = self.request(request).await?;
        let mut values = Vec::default();
        loop {
            match self.receiver.message().await? {
                Some(crate::TransactionResponse {
                    request_id,
                    response: Some(response),
                }) => {
                    check_request_id(expected_request_id, request_id)?;

                    if let crate::TransactionResponseVariant::Empty(_) = response {
                        break;
                    } else {
                        values.push(response);
                    }
                }
                _ => {
                    return Err(ClientError::UnexpectedEmptyResponse {
                        request_id: expected_request_id,
                    });
                }
            }
        }
        Ok(values)
    }

    /// Creates a new vertex. Returns whether the vertex was successfully
    /// created - if this is false, it's because a vertex with the same UUID
    /// already exists.
    ///
    /// # Arguments
    /// * `vertex`: The vertex to create.
    pub async fn create_vertex(&mut self, vertex: &indradb::Vertex) -> Result<bool, ClientError> {
        let request = crate::TransactionRequestVariant::CreateVertex(vertex.clone().into());
        Ok(self.request_single(request).await?.try_into()?)
    }

    /// Creates a new vertex with just a type specification. As opposed to
    /// `create_vertex`, this is used when you do not want to manually specify
    /// the vertex's UUID. Returns the new vertex's UUID.
    ///
    /// # Arguments
    /// * `t`: The type of the vertex to create.
    pub async fn create_vertex_from_type(&mut self, t: indradb::Identifier) -> Result<Uuid, ClientError> {
        let request = crate::TransactionRequestVariant::CreateVertexFromType(t.into());
        Ok(self.request_single(request).await?.try_into()?)
    }

    /// Gets a range of vertices specified by a query.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    pub async fn get_vertices<Q: Into<indradb::VertexQuery>>(
        &mut self,
        q: Q,
    ) -> Result<Vec<indradb::Vertex>, ClientError> {
        let request = crate::TransactionRequestVariant::GetVertices(q.into().into());
        let result: Result<Vec<indradb::Vertex>, ConversionError> = self
            .request_multi(request)
            .await?
            .into_iter()
            .map(|response| response.try_into())
            .collect();
        Ok(result?)
    }

    /// Deletes existing vertices specified by a query.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    pub async fn delete_vertices<Q: Into<indradb::VertexQuery>>(&mut self, q: Q) -> Result<(), ClientError> {
        let request = crate::TransactionRequestVariant::DeleteVertices(q.into().into());
        Ok(self.request_single(request).await?.try_into()?)
    }

    /// Gets the number of vertices in the datastore.
    pub async fn get_vertex_count(&mut self) -> Result<u64, ClientError> {
        let request = crate::TransactionRequestVariant::GetVertexCount(());
        Ok(self.request_single(request).await?.try_into()?)
    }

    /// Creates a new edge. If the edge already exists, this will update it
    /// with a new update datetime. Returns whether the edge was successfully
    /// created - if this is false, it's because one of the specified vertices
    /// is missing.
    ///
    /// # Arguments
    /// * `key`: The edge to create.
    pub async fn create_edge(&mut self, key: &indradb::EdgeKey) -> Result<bool, ClientError> {
        let request = crate::TransactionRequestVariant::CreateEdge(key.clone().into());
        Ok(self.request_single(request).await?.try_into()?)
    }

    /// Gets a range of edges specified by a query.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    pub async fn get_edges<Q: Into<indradb::EdgeQuery>>(&mut self, q: Q) -> Result<Vec<indradb::Edge>, ClientError> {
        let request = crate::TransactionRequestVariant::GetEdges(q.into().into());
        let result: Result<Vec<indradb::Edge>, ConversionError> = self
            .request_multi(request)
            .await?
            .into_iter()
            .map(|response| response.try_into())
            .collect();
        Ok(result?)
    }

    /// Deletes a set of edges specified by a query.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    pub async fn delete_edges<Q: Into<indradb::EdgeQuery>>(&mut self, q: Q) -> Result<(), ClientError> {
        let request = crate::TransactionRequestVariant::DeleteEdges(q.into().into());
        Ok(self.request_single(request).await?.try_into()?)
    }

    /// Gets the number of edges associated with a vertex.
    ///
    /// # Arguments
    /// * `id`: The id of the vertex.
    /// * `t`: Only get the count for a specified edge type.
    /// * `direction`: The direction of edges to get.
    pub async fn get_edge_count(
        &mut self,
        id: Uuid,
        t: Option<&indradb::Identifier>,
        direction: indradb::EdgeDirection,
    ) -> Result<u64, ClientError> {
        let request = crate::TransactionRequestVariant::GetEdgeCount((id, t.cloned(), direction).into());
        Ok(self.request_single(request).await?.try_into()?)
    }

    /// Gets vertex properties.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    pub async fn get_vertex_properties(
        &mut self,
        q: indradb::VertexPropertyQuery,
    ) -> Result<Vec<indradb::VertexProperty>, ClientError> {
        let request = crate::TransactionRequestVariant::GetVertexProperties(q.into());
        let result: Result<Vec<indradb::VertexProperty>, ConversionError> = self
            .request_multi(request)
            .await?
            .into_iter()
            .map(|response| response.try_into())
            .collect();
        Ok(result?)
    }

    /// Gets all vertex properties.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    pub async fn get_all_vertex_properties<Q: Into<indradb::VertexQuery>>(
        &mut self,
        q: Q,
    ) -> Result<Vec<indradb::VertexProperties>, ClientError> {
        let request = crate::TransactionRequestVariant::GetAllVertexProperties(q.into().into());
        let result: Result<Vec<indradb::VertexProperties>, ConversionError> = self
            .request_multi(request)
            .await?
            .into_iter()
            .map(|response| response.try_into())
            .collect();
        Ok(result?)
    }

    /// Sets a vertex properties.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    /// * `value`: The property value.
    pub async fn set_vertex_properties(
        &mut self,
        q: indradb::VertexPropertyQuery,
        value: &indradb::JsonValue,
    ) -> Result<(), ClientError> {
        let request = crate::TransactionRequestVariant::SetVertexProperties((q, value.clone()).into());
        Ok(self.request_single(request).await?.try_into()?)
    }

    /// Deletes vertex properties.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    pub async fn delete_vertex_properties(&mut self, q: indradb::VertexPropertyQuery) -> Result<(), ClientError> {
        let request = crate::TransactionRequestVariant::DeleteVertexProperties(q.into());
        Ok(self.request_single(request).await?.try_into()?)
    }

    /// Gets edge properties.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    pub async fn get_edge_properties(
        &mut self,
        q: indradb::EdgePropertyQuery,
    ) -> Result<Vec<indradb::EdgeProperty>, ClientError> {
        let request = crate::TransactionRequestVariant::GetEdgeProperties(q.into());
        let result: Result<Vec<indradb::EdgeProperty>, ConversionError> = self
            .request_multi(request)
            .await?
            .into_iter()
            .map(|response| response.try_into())
            .collect();
        Ok(result?)
    }

    /// Gets all edge properties.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    pub async fn get_all_edge_properties<Q: Into<indradb::EdgeQuery>>(
        &mut self,
        q: Q,
    ) -> Result<Vec<indradb::EdgeProperties>, ClientError> {
        let request = crate::TransactionRequestVariant::GetAllEdgeProperties(q.into().into());
        let result: Result<Vec<indradb::EdgeProperties>, ConversionError> = self
            .request_multi(request)
            .await?
            .into_iter()
            .map(|response| response.try_into())
            .collect();
        Ok(result?)
    }

    /// Sets edge properties.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    /// * `value`: The property value.
    pub async fn set_edge_properties(
        &mut self,
        q: indradb::EdgePropertyQuery,
        value: &indradb::JsonValue,
    ) -> Result<(), ClientError> {
        let request = crate::TransactionRequestVariant::SetEdgeProperties((q, value.clone()).into());
        Ok(self.request_single(request).await?.try_into()?)
    }

    /// Deletes edge properties.
    ///
    /// # Arguments
    /// * `q`: The query to run.
    pub async fn delete_edge_properties(&mut self, q: indradb::EdgePropertyQuery) -> Result<(), ClientError> {
        let request = crate::TransactionRequestVariant::DeleteEdgeProperties(q.into());
        Ok(self.request_single(request).await?.try_into()?)
    }
}
