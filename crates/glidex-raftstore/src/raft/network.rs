//! Raft over a [`Transport`]: the application's (glidex: HTTPS with mutual
//! TLS, `cluster::net`), or an in-process router with fault injection in
//! tests ([`crate::testing`]).

use super::types::*;
use crate::store::Schema;
use openraft::BasicNode;
use openraft::error::{Fatal, NetworkError, RPCError, RaftError, StreamingError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, SnapshotResponse, VoteRequest, VoteResponse,
};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rpc {
    Append,
    Vote,
}

impl Rpc {
    pub fn path(self) -> &'static str {
        match self {
            Rpc::Append => "/raft/append",
            Rpc::Vote => "/raft/vote",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

/// How Raft messages reach another member.
#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Send one request; the reply is the peer's JSON-encoded result.
    async fn call(
        &self,
        target: RaftId,
        addr: &str,
        rpc: Rpc,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, TransportError>;

    /// Send a snapshot: `header` (JSON, see [`SnapshotHeader`]) and the data file.
    async fn snapshot(
        &self,
        target: RaftId,
        addr: &str,
        header: Vec<u8>,
        data: PathBuf,
    ) -> Result<Vec<u8>, TransportError>;
}

#[derive(Serialize, Deserialize)]
pub struct SnapshotHeader {
    pub vote: Vote,
    pub meta: SnapshotMeta,
}

pub struct NetworkFactory<S: Schema> {
    pub transport: Arc<dyn Transport>,
    pub dir: PathBuf,
    pub schema: PhantomData<S>,
}

pub struct Peer<S: Schema> {
    transport: Arc<dyn Transport>,
    target: RaftId,
    addr: String,
    dir: PathBuf,
    schema: PhantomData<S>,
}

impl<S: Schema> RaftNetworkFactory<TypeConfig<S>> for NetworkFactory<S> {
    type Network = Peer<S>;

    async fn new_client(&mut self, target: RaftId, node: &BasicNode) -> Peer<S> {
        Peer {
            transport: self.transport.clone(),
            target,
            addr: node.addr.clone(),
            dir: self.dir.clone(),
            schema: PhantomData,
        }
    }
}

type Reply<T> = Result<T, RaftError<RaftId>>;

impl<S: Schema> Peer<S> {
    async fn rpc<Req: Serialize, Resp: serde::de::DeserializeOwned>(
        &self,
        rpc: Rpc,
        req: &Req,
    ) -> Result<Resp, RPCError<RaftId, BasicNode, RaftError<RaftId>>> {
        let body = serde_json::to_vec(req).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        let out = self
            .transport
            .call(self.target, &self.addr, rpc, body)
            .await
            .map_err(|e| RPCError::Unreachable(Unreachable::new(&e)))?;
        let reply: Reply<Resp> =
            serde_json::from_slice(&out).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        reply.map_err(|e| RPCError::RemoteError(openraft::error::RemoteError::new(self.target, e)))
    }
}

impl<S: Schema> RaftNetwork<TypeConfig<S>> for Peer<S> {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig<S>>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<RaftId>, RPCError<RaftId, BasicNode, RaftError<RaftId>>> {
        self.rpc(Rpc::Append, &rpc).await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<RaftId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<RaftId>, RPCError<RaftId, BasicNode, RaftError<RaftId>>> {
        self.rpc(Rpc::Vote, &rpc).await
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote,
        snapshot: openraft::Snapshot<TypeConfig<S>>,
        _cancel: impl Future<Output = openraft::error::ReplicationClosed>
        + openraft::OptionalSend
        + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<RaftId>, StreamingError<TypeConfig<S>, Fatal<RaftId>>> {
        let view = snapshot.snapshot.view.ok_or_else(|| {
            StreamingError::Network(NetworkError::new(&TransportError(
                "snapshot has no data".into(),
            )))
        })?;
        let mut nonce = [0u8; 6];
        getrandom::fill(&mut nonce).expect("system randomness");
        // Unique per send: a retry must not trample a transfer still in flight.
        let path = self.dir.join(format!(
            "send-{}-{}-{}.snap",
            self.target,
            snapshot.meta.snapshot_id,
            hex(&nonce)
        ));
        let out = path.clone();
        // One read transaction, streamed to a file: bounded by disk, not memory.
        tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
            std::fs::create_dir_all(out.parent().unwrap())?;
            let mut w = std::io::BufWriter::new(std::fs::File::create(&out)?);
            view.write_to(&mut w).map_err(std::io::Error::other)?;
            Ok(())
        })
        .await
        .map_err(|e| StreamingError::Network(NetworkError::new(&e)))?
        .map_err(|e| StreamingError::Network(NetworkError::new(&e)))?;
        let header = serde_json::to_vec(&SnapshotHeader {
            vote,
            meta: snapshot.meta,
        })
        .map_err(|e| StreamingError::Network(NetworkError::new(&e)))?;
        let reply = self
            .transport
            .snapshot(self.target, &self.addr, header, path.clone())
            .await;
        let _ = std::fs::remove_file(&path);
        let reply = reply.map_err(|e| StreamingError::Unreachable(Unreachable::new(&e)))?;
        let reply: Result<SnapshotResponse<RaftId>, Fatal<RaftId>> = serde_json::from_slice(&reply)
            .map_err(|e| StreamingError::Network(NetworkError::new(&e)))?;
        reply.map_err(|e| {
            StreamingError::RemoteError(openraft::error::RemoteError::new(self.target, e))
        })
    }
}

/// The receiving side: what a peer's request is handed to.
pub struct RaftService<S: Schema> {
    pub raft: Raft<S>,
    pub dir: PathBuf,
}

impl<S: Schema> RaftService<S> {
    pub async fn handle(&self, rpc: Rpc, body: &[u8]) -> Result<Vec<u8>, TransportError> {
        let err = |e: serde_json::Error| TransportError(e.to_string());
        match rpc {
            Rpc::Append => {
                let req: AppendEntriesRequest<TypeConfig<S>> =
                    serde_json::from_slice(body).map_err(err)?;
                serde_json::to_vec(&self.raft.append_entries(req).await).map_err(err)
            }
            Rpc::Vote => {
                let req: VoteRequest<RaftId> = serde_json::from_slice(body).map_err(err)?;
                serde_json::to_vec(&self.raft.vote(req).await).map_err(err)
            }
        }
    }

    /// `data` is the received snapshot file; it is consumed.
    pub async fn handle_snapshot(
        &self,
        header: &[u8],
        data: PathBuf,
    ) -> Result<Vec<u8>, TransportError> {
        let err = |e: serde_json::Error| TransportError(e.to_string());
        let h: SnapshotHeader = serde_json::from_slice(header).map_err(err)?;
        let snapshot = openraft::Snapshot {
            meta: h.meta,
            snapshot: Box::new(SnapshotHandle {
                view: None,
                file: Some(data),
            }),
        };
        let res = self.raft.install_full_snapshot(h.vote, snapshot).await;
        serde_json::to_vec(&res).map_err(err)
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
