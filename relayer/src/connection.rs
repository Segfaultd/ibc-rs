use core::marker::PhantomData;
use std::time::Duration;

use crate::chain::counterparty::connection_state_on_destination;
use crate::util::retry::RetryResult;
use flex_error::define_error;
use ibc_proto::ibc::core::connection::v1::QueryConnectionsRequest;
use prost_types::Any;
use serde::Serialize;
use tracing::debug;
use tracing::{error, warn};

use ibc::events::IbcEvent;
use ibc::ics02_client::height::Height;
use ibc::ics03_connection::connection::{self, State};
use ibc::ics03_connection::events::TaggedAttributes;
use ibc::ics03_connection::msgs::conn_open_ack::MsgConnectionOpenAck;
use ibc::ics03_connection::msgs::conn_open_confirm::MsgConnectionOpenConfirm;
use ibc::ics03_connection::msgs::conn_open_init::MsgConnectionOpenInit;
use ibc::ics03_connection::msgs::conn_open_try::MsgConnectionOpenTry;
use ibc::ics03_connection::version::Version;
use ibc::ics23_commitment::commitment::CommitmentPrefix;
use ibc::ics24_host::identifier::{ChainId, ClientId, ConnectionId};
use ibc::tagged::{DualTagged, Tagged};
use ibc::timestamp::ZERO_DURATION;
use ibc::tx_msg::Msg;

use crate::chain::handle::ChainHandle;
use crate::error::Error as RelayerError;
use crate::foreign_client::{ForeignClient, ForeignClientError};
use crate::object::Connection as WorkerConnectionObject;
use crate::supervisor::Error as SupervisorError;

/// Maximum value allowed for packet delay on any new connection that the relayer establishes.
pub const MAX_PACKET_DELAY: Duration = Duration::from_secs(120);

const MAX_RETRIES: usize = 5;

define_error! {
    ConnectionError {
        Relayer
            [ RelayerError ]
            |_| { "relayer error" },

        MissingLocalConnectionId
            |_| { "failed due to missing local channel id" },

        MissingCounterpartyConnectionIdField
            { counterparty: connection::Counterparty }
            |e| {
                format!("the connection end has no connection id field in the counterparty: {:?}",
                    e.counterparty)
            },

        MissingCounterpartyConnectionId
            |_| { "failed due to missing counterparty connection id" },

        ChainQuery
            { chain_id: ChainId }
            [ RelayerError ]
            |e| {
                format!("failed during a query to chain id {0}", e.chain_id)
            },

        ConnectionQuery
            { connection_id: ConnectionId }
            [ RelayerError ]
            |e| {
                format!("failed to query the connection for {}", e.connection_id)
            },

        ClientOperation
            {
                client_id: ClientId,
                chain_id: ChainId,
            }
            [ ForeignClientError ]
            |e| {
                format!("failed during an operation on client ({0}) hosted by chain ({1})",
                    e.client_id, e.chain_id)
            },

        Submit
            { chain_id: ChainId }
            [ RelayerError ]
            |e| {
                format!("failed during a transaction submission step to chain id {0}",
                    e.chain_id)
            },

        MaxDelayPeriod
            { delay_period: Duration }
            |e| {
                format!("Invalid delay period '{:?}': should be at max '{:?}'",
                    e.delay_period, MAX_PACKET_DELAY)
            },

        InvalidEvent
            { event: IbcEvent }
            |e| {
                format!("a connection object cannot be built from {}",
                    e.event)
            },

        TxResponse
            { event: String }
            |e| {
                format!("tx response event consists of an error: {}",
                    e.event)
            },

        ConnectionClientIdMismatch
            {
                client_id: ClientId,
                foreign_client_id: ClientId
            }
            |e| {
                format!("the client id in the connection end ({}) does not match the foreign client id ({})",
                    e.client_id, e.foreign_client_id)
            },

        ChainIdMismatch
            {
                source_chain_id: ChainId,
                destination_chain_id: ChainId
            }
            |e| {
                format!("the source chain of client a ({}) does not not match the destination chain of client b ({})",
                    e.source_chain_id, e.destination_chain_id)
            },

        ConnectionNotOpen
            {
                state: State,
            }
            |e| {
                format!("the connection end is expected to be in state 'Open'; found state: {:?}",
                    e.state)
            },

        MaxRetry
            |_| {
                format!("Failed to finish connection handshake in {:?} iterations",
                    MAX_RETRIES)
            },

        Supervisor
            [ SupervisorError ]
            |_| { "supervisor error" },

        MissingConnectionId
            {
                chain_id: ChainId,
            }
            |e| {
                format!("missing connection on source chain {}",
                    e.chain_id)
            },

        Signer
            { chain_id: ChainId }
            [ RelayerError ]
            |e| {
                format!("failed while fetching the signer for chain ({})",
                    e.chain_id)
            },

        MissingConnectionIdFromEvent
            |_| { "cannot extract connection_id from result" },

        MissingConnectionInitEvent
            |_| { "no conn init event was in the response" },

        MissingConnectionTryEvent
            |_| { "no conn try event was in the response" },

        MissingConnectionAckEvent
            |_| { "no conn ack event was in the response" },

        MissingConnectionConfirmEvent
            |_| { "no conn confirm event was in the response" },

        ConnectionProof
            [ RelayerError ]
            |_| { "failed to build connection proofs" },

        ConnectionAlreadyExist
            { connection_id: ConnectionId }
            |e| {
                format!("connection {} already exist in an incompatible state", e.connection_id)
            },

    }
}

#[derive(Clone, Debug)]
pub struct ConnectionSide<Chain, CounterpartyChain>
where
    Chain: ChainHandle<CounterpartyChain>,
{
    pub(crate) chain: Chain,
    client_id: Tagged<Chain, ClientId>,
    connection_id: Option<Tagged<Chain, ConnectionId>>,
    phantom: PhantomData<CounterpartyChain>,
}

#[derive(Debug, Clone)]
pub struct ConnectionEnd<ChainA, ChainB>(pub DualTagged<ChainA, ChainB, connection::ConnectionEnd>);

#[derive(Debug, Clone)]
pub struct IdentifiedConnectionEnd<ChainA, ChainB>(
    pub DualTagged<ChainA, ChainB, connection::IdentifiedConnectionEnd>,
);

#[derive(Debug, Clone)]
pub struct Counterparty<Chain>(pub Tagged<Chain, connection::Counterparty>);

impl<ChainA, ChainB> ConnectionEnd<ChainA, ChainB> {
    pub fn new(
        state: Tagged<ChainA, State>,
        client_id: Tagged<ChainA, ClientId>,
        counterparty: Counterparty<ChainB>,
        versions: Vec<Tagged<ChainB, Version>>,
        delay_period: Duration,
    ) -> Self {
        Self(DualTagged::new(connection::ConnectionEnd::new(
            state.untag(),
            client_id.untag(),
            counterparty.0.untag(),
            versions.into_iter().map(Tagged::untag).collect(),
            delay_period,
        )))
    }

    pub fn tag(connection: connection::ConnectionEnd) -> Self {
        Self(DualTagged::new(connection))
    }

    pub fn state(&self) -> Tagged<ChainA, State> {
        self.0.map(|c| c.state.clone())
    }

    pub fn client_id(&self) -> Tagged<ChainA, ClientId> {
        self.0.map(|c| c.client_id().clone())
    }

    pub fn counterparty(&self) -> Counterparty<ChainB> {
        Counterparty(self.0.map_flipped(|c| c.counterparty().clone()))
    }

    pub fn versions(&self) -> Vec<Tagged<ChainA, Version>> {
        self.value()
            .versions()
            .into_iter()
            .map(Tagged::new)
            .collect()
    }

    pub fn delay_period(&self) -> Duration {
        self.value().delay_period().clone()
    }

    pub fn state_matches(&self, other: Tagged<ChainA, State>) -> bool {
        self.value().state_matches(other.value())
    }

    pub fn client_id_matches(&self, other: Tagged<ChainA, ClientId>) -> bool {
        self.value().client_id_matches(other.value())
    }

    pub fn value(&self) -> &connection::ConnectionEnd {
        self.0.value()
    }
}

impl<ChainA, ChainB> IdentifiedConnectionEnd<ChainA, ChainB> {
    pub fn new(
        connection_id: Tagged<ChainA, ConnectionId>,
        connection_end: ConnectionEnd<ChainA, ChainB>,
    ) -> Self {
        Self(DualTagged::new(connection::IdentifiedConnectionEnd::new(
            connection_id.untag(),
            connection_end.0.untag(),
        )))
    }

    pub fn tag(connection: connection::IdentifiedConnectionEnd) -> Self {
        Self(DualTagged::new(connection))
    }

    pub fn connection_id(&self) -> Tagged<ChainA, ConnectionId> {
        self.0.map(|c| c.connection_id.clone())
    }

    pub fn connection_end(&self) -> ConnectionEnd<ChainA, ChainB> {
        ConnectionEnd(self.0.dual_map(|c| c.connection_end.clone()))
    }

    pub fn counterparty(&self) -> Counterparty<ChainB> {
        self.connection_end().counterparty()
    }
}

impl<Chain> Counterparty<Chain> {
    pub fn new(
        client_id: Tagged<Chain, ClientId>,
        connection_id: Option<Tagged<Chain, ConnectionId>>,
        prefix: Tagged<Chain, CommitmentPrefix>,
    ) -> Self {
        Self(Tagged::new(connection::Counterparty::new(
            client_id.untag(),
            connection_id.map(Tagged::untag),
            prefix.untag(),
        )))
    }

    pub fn tag(counterparty: connection::Counterparty) -> Self {
        Self(Tagged::new(counterparty))
    }

    pub fn client_id(&self) -> Tagged<Chain, ClientId> {
        self.0.map(|c| c.client_id().clone())
    }

    pub fn connection_id(&self) -> Option<Tagged<Chain, ConnectionId>> {
        self.0.map(|c| c.connection_id().clone()).transpose()
    }

    pub fn commitment_prefix(&self) -> Tagged<Chain, CommitmentPrefix> {
        self.0.map(|c| c.prefix().clone())
    }
}

impl<Chain, CounterpartyChain> ConnectionSide<Chain, CounterpartyChain>
where
    Chain: ChainHandle<CounterpartyChain>,
{
    pub fn new(
        chain: Chain,
        client_id: Tagged<Chain, ClientId>,
        connection_id: Option<Tagged<Chain, ConnectionId>>,
    ) -> Self {
        Self {
            chain,
            client_id,
            connection_id,
            phantom: PhantomData,
        }
    }
    pub fn connection_id(&self) -> Option<Tagged<Chain, ConnectionId>> {
        self.connection_id.clone()
    }
}

impl<Chain, CounterpartyChain> Serialize for ConnectionSide<Chain, CounterpartyChain>
where
    Chain: ChainHandle<CounterpartyChain>,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Debug, Serialize)]
        struct ConnectionSide<'a> {
            client_id: &'a ClientId,
            connection_id: &'a Option<ConnectionId>,
        }

        let value = ConnectionSide {
            client_id: self.client_id.value(),
            connection_id: &self.connection_id.map(Tagged::untag),
        };

        value.serialize(serializer)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Connection<ChainA, ChainB>
where
    ChainA: ChainHandle<ChainB>,
    ChainB: ChainHandle<ChainA>,
{
    pub delay_period: Duration,
    pub a_side: ConnectionSide<ChainA, ChainB>,
    pub b_side: ConnectionSide<ChainB, ChainA>,
}

impl<ChainA, ChainB> Connection<ChainA, ChainB>
where
    ChainA: ChainHandle<ChainB>,
    ChainB: ChainHandle<ChainA>,
{
    /// Create a new connection, ensuring that the handshake has succeeded and the two connection
    /// ends exist on each side.
    pub fn new(
        a_client: ForeignClient<ChainA, ChainB>,
        b_client: ForeignClient<ChainB, ChainA>,
        delay_period: Duration,
    ) -> Result<Self, ConnectionError> {
        Self::validate_clients(&a_client, &b_client)?;

        // Validate the delay period against the upper bound
        if delay_period > MAX_PACKET_DELAY {
            return Err(ConnectionError::max_delay_period(delay_period));
        }

        let mut c = Self {
            delay_period,
            a_side: ConnectionSide::new(
                a_client.dst_chain(),
                a_client.id().clone(),
                Default::default(),
            ),
            b_side: ConnectionSide::new(
                b_client.dst_chain(),
                b_client.id().clone(),
                Default::default(),
            ),
        };

        c.handshake()?;

        Ok(c)
    }

    pub fn restore_from_event(
        chain: ChainA,
        counterparty_chain: ChainB,
        connection_open_event: DualTagged<ChainA, ChainB, IbcEvent>,
    ) -> Result<Connection<ChainA, ChainB>, ConnectionError> {
        let connection_event_attributes = connection_open_event
            .dual_map(|e| e.connection_attributes())
            .transpose()
            .map(TaggedAttributes)
            .ok_or_else(|| ConnectionError::invalid_event(connection_open_event.value().clone()))?;

        let connection_id = connection_event_attributes.connection_id();

        let counterparty_connection_id = connection_event_attributes.counterparty_connection_id();

        let client_id = connection_event_attributes.client_id();
        let counterparty_client_id = connection_event_attributes.counterparty_client_id();

        Ok(Connection {
            // The event does not include the connection delay.
            delay_period: Default::default(),
            a_side: ConnectionSide::new(chain, client_id, connection_id),
            b_side: ConnectionSide::new(
                counterparty_chain,
                counterparty_client_id,
                counterparty_connection_id,
            ),
        })
    }

    /// Recreates a 'Connection' object from the worker's object built from chain state scanning.
    /// The connection must exist on chain.
    pub fn restore_from_state(
        chain: ChainA,
        counterparty_chain: ChainB,
        connection: WorkerConnectionObject<ChainB, ChainA>,
        height: Tagged<ChainA, Height>,
    ) -> Result<(Connection<ChainA, ChainB>, Tagged<ChainA, State>), ConnectionError> {
        let a_connection = chain
            .query_connection(connection.src_connection_id.clone(), height)
            .map_err(ConnectionError::relayer)?;

        let client_id = a_connection.client_id();
        let delay_period = a_connection.delay_period();

        let counterparty_connection_id = a_connection.counterparty().connection_id();

        let counterparty_client_id = a_connection.counterparty().client_id();

        let mut handshake_connection = Connection {
            delay_period,
            a_side: ConnectionSide::new(
                chain,
                client_id.clone(),
                Some(connection.src_connection_id.clone()),
            ),
            b_side: ConnectionSide::new(
                counterparty_chain.clone(),
                counterparty_client_id.clone(),
                counterparty_connection_id.clone(),
            ),
        };

        if a_connection.state_matches(Tagged::new(State::Init))
            && counterparty_connection_id.is_none()
        {
            let req = QueryConnectionsRequest {
                pagination: ibc_proto::cosmos::base::query::pagination::all(),
            };
            let connections: Vec<IdentifiedConnectionEnd<ChainB, ChainA>> = counterparty_chain
                .query_connections(req)
                .map_err(ConnectionError::relayer)?;

            for conn in connections {
                if !conn
                    .connection_end()
                    .client_id_matches(a_connection.counterparty().client_id())
                {
                    continue;
                }
                if let Some(remote_connection_id) =
                    conn.connection_end().counterparty().connection_id()
                {
                    if remote_connection_id == connection.src_connection_id {
                        handshake_connection.b_side.connection_id = Some(conn.connection_id());
                        break;
                    }
                }
            }
        }

        Ok((handshake_connection, a_connection.state()))
    }

    pub fn find(
        a_client: ForeignClient<ChainA, ChainB>,
        b_client: ForeignClient<ChainB, ChainA>,
        conn_end_a: IdentifiedConnectionEnd<ChainA, ChainB>,
    ) -> Result<Connection<ChainA, ChainB>, ConnectionError> {
        Self::validate_clients(&a_client, &b_client)?;

        let a_client_id = a_client.id();
        let b_client_id = b_client.id();

        let end_a = conn_end_a.connection_end();
        let end_b = end_a.counterparty();

        // Validate the connection end
        if end_a.client_id().ne(&a_client_id) {
            return Err(ConnectionError::connection_client_id_mismatch(
                end_a.client_id().untag(),
                a_client.id().untag(),
            ));
        }

        if end_a.counterparty().client_id() != b_client.id() {
            return Err(ConnectionError::connection_client_id_mismatch(
                end_a.counterparty().client_id().untag(),
                b_client.id().untag(),
            ));
        }

        if !end_a.state_matches(Tagged::new(State::Open)) {
            return Err(ConnectionError::connection_not_open(end_a.state().untag()));
        }

        let b_conn_id = end_b.connection_id().ok_or_else(|| {
            ConnectionError::missing_counterparty_connection_id_field(end_b.0.untag())
        })?;

        let c = Connection {
            delay_period: end_a.delay_period(),
            a_side: ConnectionSide::new(
                a_client.dst_chain.clone(),
                a_client.id,
                Some(conn_end_a.connection_id().clone()),
            ),
            b_side: ConnectionSide::new(b_client.dst_chain.clone(), b_client.id, Some(b_conn_id)),
        };

        Ok(c)
    }

    // Verifies that the two clients are mutually consistent, i.e., they serve the same two chains.
    fn validate_clients(
        a_client: &ForeignClient<ChainA, ChainB>,
        b_client: &ForeignClient<ChainB, ChainA>,
    ) -> Result<(), ConnectionError> {
        if a_client.src_chain().id() != b_client.dst_chain().id() {
            return Err(ConnectionError::chain_id_mismatch(
                a_client.src_chain().id().untag(),
                b_client.dst_chain().id().untag(),
            ));
        }

        if a_client.dst_chain().id() != b_client.src_chain().id() {
            return Err(ConnectionError::chain_id_mismatch(
                a_client.dst_chain().id().untag(),
                b_client.src_chain().id().untag(),
            ));
        }

        Ok(())
    }

    pub fn src_chain(&self) -> &ChainA {
        &self.a_side.chain
    }

    pub fn dst_chain(&self) -> &ChainB {
        &self.b_side.chain
    }

    pub fn src_client_id(&self) -> Tagged<ChainA, ClientId> {
        self.a_side.client_id
    }

    pub fn dst_client_id(&self) -> Tagged<ChainB, ClientId> {
        self.b_side.client_id
    }

    pub fn src_connection_id(&self) -> Option<Tagged<ChainA, ConnectionId>> {
        self.a_side.connection_id()
    }

    pub fn dst_connection_id(&self) -> Option<Tagged<ChainB, ConnectionId>> {
        self.b_side.connection_id()
    }

    pub fn flipped(&self) -> Connection<ChainB, ChainA> {
        Connection {
            a_side: self.b_side.clone(),
            b_side: self.a_side.clone(),
            delay_period: self.delay_period,
        }
    }

    /// Executes a connection handshake protocol (ICS 003) for this connection object
    fn handshake(&mut self) -> Result<(), ConnectionError> {
        let done = '🥂';

        let a_chain = self.a_side.chain.clone();
        let b_chain = self.b_side.chain.clone();

        // Try connOpenInit on a_chain
        let mut counter = 0;
        while counter < MAX_RETRIES {
            counter += 1;
            match self.flipped().build_conn_init_and_send() {
                Err(e) => {
                    error!("Failed ConnInit {:?}: {}", self.a_side, e);
                    continue;
                }
                Ok(result) => {
                    let connection_id = result.map(|e| extract_connection_id(e)).transpose()?;

                    self.a_side.connection_id = Some(connection_id);
                    println!("🥂  {} => {:#?}\n", self.a_side.chain.id(), result);
                    break;
                }
            }
        }

        // Try connOpenTry on b_chain
        counter = 0;
        while counter < MAX_RETRIES {
            counter += 1;
            match self.build_conn_try_and_send() {
                Err(e) => {
                    error!("Failed ConnTry {:?}: {}", self.b_side, e);
                    continue;
                }
                Ok(result) => {
                    let connection_id = result.map(|e| extract_connection_id(e)).transpose()?;

                    self.b_side.connection_id = Some(connection_id);
                    println!("{}  {} => {:#?}\n", done, self.b_side.chain.id(), result);
                    break;
                }
            }
        }

        counter = 0;
        while counter < MAX_RETRIES {
            counter += 1;

            let src_connection_id = self
                .src_connection_id()
                .ok_or_else(ConnectionError::missing_local_connection_id)?;
            let dst_connection_id = self
                .dst_connection_id()
                .ok_or_else(ConnectionError::missing_counterparty_connection_id)?;

            // Continue loop if query error
            let a_connection = a_chain.query_connection(src_connection_id, Height::tagged_zero());
            if a_connection.is_err() {
                continue;
            }
            let b_connection = b_chain.query_connection(dst_connection_id, Height::tagged_zero());
            if b_connection.is_err() {
                continue;
            }

            match (
                a_connection.unwrap().state().untag(),
                b_connection.unwrap().state().untag(),
            ) {
                (State::Init, State::TryOpen) | (State::TryOpen, State::TryOpen) => {
                    // Ack to a_chain
                    match self.flipped().build_conn_ack_and_send() {
                        Err(e) => error!("Failed ConnAck {:?}: {}", self.a_side, e),
                        Ok(event) => {
                            println!("{}  {} => {:#?}\n", done, self.a_side.chain.id(), event)
                        }
                    }
                }
                (State::Open, State::TryOpen) => {
                    // Confirm to b_chain
                    match self.build_conn_confirm_and_send() {
                        Err(e) => error!("Failed ConnConfirm {:?}: {}", self.b_side, e),
                        Ok(event) => {
                            println!("{}  {} => {:#?}\n", done, self.b_side.chain.id(), event)
                        }
                    }
                }
                (State::TryOpen, State::Open) => {
                    // Confirm to a_chain
                    match self.flipped().build_conn_confirm_and_send() {
                        Err(e) => error!("Failed ConnConfirm {:?}: {}", self.a_side, e),
                        Ok(event) => {
                            println!("{}  {} => {:#?}\n", done, self.a_side.chain.id(), event)
                        }
                    }
                }
                (State::Open, State::Open) => {
                    println!(
                        "{0}{0}{0}  Connection handshake finished for [{1:#?}]\n",
                        done, self
                    );
                    return Ok(());
                }
                _ => {}
            }
        }

        Err(ConnectionError::max_retry())
    }

    pub fn counterparty_state(&self) -> Result<Tagged<ChainB, State>, ConnectionError> {
        // Source connection ID must be specified
        let connection_id = self
            .src_connection_id()
            .ok_or_else(ConnectionError::missing_local_connection_id)?;

        let connection_end = self
            .src_chain()
            .query_connection(connection_id, Height::tagged_zero())
            .map_err(|e| ConnectionError::connection_query(connection_id.untag(), e))?;

        let connection = IdentifiedConnectionEnd::new(connection_id.clone(), connection_end);

        connection_state_on_destination(connection, &self.dst_chain())
            .map_err(ConnectionError::supervisor)
    }

    pub fn handshake_step(
        &mut self,
        state: State,
    ) -> Result<Vec<Tagged<ChainB, IbcEvent>>, ConnectionError> {
        match (state, self.counterparty_state()?.untag()) {
            (State::Init, State::Uninitialized) => Ok(vec![self.build_conn_try_and_send()?]),
            (State::Init, State::Init) => Ok(vec![self.build_conn_try_and_send()?]),
            (State::TryOpen, State::Init) => Ok(vec![self.build_conn_ack_and_send()?]),
            (State::TryOpen, State::TryOpen) => Ok(vec![self.build_conn_ack_and_send()?]),
            (State::Open, State::TryOpen) => Ok(vec![self.build_conn_confirm_and_send()?]),
            _ => Ok(vec![]),
        }
    }

    pub fn step_state(&mut self, state: State, index: u64) -> RetryResult<(), u64> {
        let done = '🥳';

        match self.handshake_step(state) {
            Err(e) => {
                error!("failed {:?} with error {}", state, e);
                RetryResult::Retry(index)
            }
            Ok(ev) => {
                debug!("{} => {:#?}\n", done, ev);
                RetryResult::Ok(())
            }
        }
    }

    pub fn step_event(&mut self, event: IbcEvent, index: u64) -> RetryResult<(), u64> {
        let state = match event {
            IbcEvent::OpenInitConnection(_) => State::Init,
            IbcEvent::OpenTryConnection(_) => State::TryOpen,
            IbcEvent::OpenAckConnection(_) => State::Open,
            IbcEvent::OpenConfirmConnection(_) => State::Open,
            _ => State::Uninitialized,
        };

        self.step_state(state, index)
    }

    /// Retrieves the connection from destination and compares against the expected connection
    /// built from the message type (`msg_type`) and options (`opts`).
    /// If the expected and the destination connections are compatible, it returns the expected connection
    fn validated_expected_connection(
        &self,
        msg_type: ConnectionMsgType,
    ) -> Result<ConnectionEnd<ChainB, ChainA>, ConnectionError> {
        let dst_connection_id = self
            .dst_connection_id()
            .ok_or_else(ConnectionError::missing_counterparty_connection_id)?;

        let prefix = self
            .src_chain()
            .query_commitment_prefix()
            .map_err(|e| ConnectionError::chain_query(self.src_chain().id().untag(), e))?;

        // If there is a connection present on the destination chain, it should look like this:
        let counterparty = Counterparty::new(
            self.src_client_id().clone(),
            self.src_connection_id(),
            prefix,
        );

        // The highest expected state, depends on the message type:
        let highest_state = match msg_type {
            ConnectionMsgType::OpenAck => State::TryOpen,
            ConnectionMsgType::OpenConfirm => State::TryOpen,
            _ => State::Uninitialized,
        };

        let versions = self
            .src_chain()
            .query_compatible_versions()
            .map_err(|e| ConnectionError::chain_query(self.src_chain().id().untag(), e))?;

        let dst_expected_connection = ConnectionEnd::new(
            Tagged::new(highest_state),
            self.dst_client_id().clone(),
            counterparty,
            versions,
            ZERO_DURATION,
        );

        // Retrieve existing connection if any
        let dst_connection = self
            .dst_chain()
            .query_connection(dst_connection_id, Height::tagged_zero())
            .map_err(|e| ConnectionError::chain_query(self.dst_chain().id().untag(), e))?;

        // Check if a connection is expected to exist on destination chain
        // A connection must exist on destination chain for Ack and Confirm Tx-es to succeed
        if dst_connection.state_matches(Tagged::new(State::Uninitialized)) {
            return Err(ConnectionError::missing_connection_id(
                self.dst_chain().id().untag(),
            ));
        }

        check_destination_connection_state(
            dst_connection_id.clone(),
            dst_connection,
            dst_expected_connection.clone(),
        )?;

        Ok(dst_expected_connection)
    }

    pub fn build_update_client_on_src(
        &self,
        height: Tagged<ChainB, Height>,
    ) -> Result<Vec<Tagged<ChainA, Any>>, ConnectionError> {
        let client = self.restore_src_client();
        client.build_update_client(height).map_err(|e| {
            ConnectionError::client_operation(
                self.src_client_id().untag(),
                self.src_chain().id().untag(),
                e,
            )
        })
    }

    pub fn build_update_client_on_dst(
        &self,
        height: Tagged<ChainA, Height>,
    ) -> Result<Vec<Tagged<ChainB, Any>>, ConnectionError> {
        let client = self.restore_dst_client();
        client.build_update_client(height).map_err(|e| {
            ConnectionError::client_operation(
                self.dst_client_id().untag(),
                self.dst_chain().id().untag(),
                e,
            )
        })
    }

    pub fn build_conn_init(&self) -> Result<Vec<Tagged<ChainB, Any>>, ConnectionError> {
        // Get signer
        let signer = self
            .dst_chain()
            .get_signer()
            .map_err(|e| ConnectionError::signer(self.dst_chain().id().untag(), e))?;

        let prefix = self
            .src_chain()
            .query_commitment_prefix()
            .map_err(|e| ConnectionError::chain_query(self.src_chain().id().untag(), e))?;

        let counterparty = Counterparty::new(self.src_client_id(), None, prefix);

        let version = self
            .dst_chain()
            .query_compatible_versions()
            .map_err(|e| ConnectionError::chain_query(self.dst_chain().id().untag(), e))?[0]
            .clone();

        // Build the domain type message
        let new_msg = MsgConnectionOpenInit::tagged_new(
            self.dst_client_id().clone(),
            counterparty.0,
            version,
            self.delay_period,
            signer,
        );

        Ok(vec![new_msg.map_into(Msg::to_any)])
    }

    pub fn build_conn_init_and_send(&self) -> Result<Tagged<ChainB, IbcEvent>, ConnectionError> {
        let dst_msgs = self.build_conn_init()?;

        let events = self
            .dst_chain()
            .send_msgs(dst_msgs)
            .map_err(|e| ConnectionError::submit(self.dst_chain().id().untag(), e))?;

        // Find the relevant event for connection init
        for event in events {
            match event.value() {
                IbcEvent::OpenInitConnection(_) => {
                    return Ok(event);
                }
                IbcEvent::ChainError(e) => {
                    return Err(ConnectionError::tx_response(e.clone()));
                }
                _ => {}
            }
        }

        Err(ConnectionError::missing_connection_init_event())
    }

    /// Attempts to build a MsgConnOpenTry.
    pub fn build_conn_try(&self) -> Result<Vec<Tagged<ChainB, Any>>, ConnectionError> {
        let src_connection_id = self
            .src_connection_id()
            .ok_or_else(ConnectionError::missing_local_connection_id)?;

        let src_connection = self
            .src_chain()
            .query_connection(src_connection_id, Height::tagged_zero())
            .map_err(|e| ConnectionError::chain_query(self.src_chain().id().untag(), e))?;

        // TODO - check that the src connection is consistent with the try options

        // Cross-check the delay_period
        let delay = if src_connection.delay_period() != self.delay_period {
            warn!("`delay_period` for ConnectionEnd @{} is {}s; delay period on local Connection object is set to {}s",
                self.src_chain().id(), src_connection.delay_period().as_secs_f64(), self.delay_period.as_secs_f64());

            warn!(
                "Overriding delay period for local connection object to {}s",
                src_connection.delay_period().as_secs_f64()
            );

            src_connection.delay_period()
        } else {
            self.delay_period
        };

        // Build add send the message(s) for updating client on source
        // TODO - add check if update client is required
        let src_client_target_height = self
            .dst_chain()
            .query_latest_height()
            .map_err(|e| ConnectionError::chain_query(self.dst_chain().id().untag(), e))?;
        let client_msgs = self.build_update_client_on_src(src_client_target_height)?;
        self.src_chain()
            .send_msgs(client_msgs)
            .map_err(|e| ConnectionError::submit(self.src_chain().id().untag(), e))?;

        let query_height = self
            .src_chain()
            .query_latest_height()
            .map_err(|e| ConnectionError::chain_query(self.src_chain().id().untag(), e))?;
        let (client_state, proofs) = self
            .src_chain()
            .build_connection_proofs_and_client_state(
                ConnectionMsgType::OpenTry,
                src_connection_id,
                self.src_client_id(),
                query_height,
            )
            .map_err(ConnectionError::connection_proof)?;

        // Build message(s) for updating client on destination
        let mut msgs = self.build_update_client_on_dst(proofs.map(|p| p.height()))?;

        let counterparty_versions = if src_connection.versions().is_empty() {
            self.src_chain()
                .query_compatible_versions()
                .map_err(|e| ConnectionError::chain_query(self.src_chain().id().untag(), e))?
        } else {
            src_connection.versions()
        };

        // Get signer
        let signer = self
            .dst_chain()
            .get_signer()
            .map_err(|e| ConnectionError::signer(self.dst_chain().id().untag(), e))?;

        let prefix = self
            .src_chain()
            .query_commitment_prefix()
            .map_err(|e| ConnectionError::chain_query(self.src_chain().id().untag(), e))?;

        let counterparty = Counterparty::new(
            self.src_client_id().clone(),
            self.src_connection_id(),
            prefix,
        );

        let previous_connection_id = if src_connection.counterparty().connection_id().is_none() {
            self.b_side.connection_id()
        } else {
            src_connection.counterparty().connection_id()
        };

        let new_msg = MsgConnectionOpenTry::tagged_new(
            previous_connection_id,
            self.dst_client_id(),
            client_state,
            counterparty.0,
            counterparty_versions,
            proofs,
            delay,
            signer,
        );

        msgs.push(new_msg.map_into(Msg::to_any));
        Ok(msgs)
    }

    pub fn build_conn_try_and_send(&self) -> Result<Tagged<ChainB, IbcEvent>, ConnectionError> {
        let dst_msgs = self.build_conn_try()?;

        let events = self
            .dst_chain()
            .send_msgs(dst_msgs)
            .map_err(|e| ConnectionError::submit(self.dst_chain().id().untag(), e))?;

        // Find the relevant event for connection try transaction
        for event in events {
            match event.value() {
                IbcEvent::OpenTryConnection(_) => {
                    return Ok(event);
                }
                IbcEvent::ChainError(e) => {
                    return Err(ConnectionError::tx_response(e.clone()));
                }
                _ => {}
            }
        }

        Err(ConnectionError::missing_connection_try_event())
    }

    /// Attempts to build a MsgConnOpenAck.
    pub fn build_conn_ack(&self) -> Result<Vec<Tagged<ChainB, Any>>, ConnectionError> {
        let src_connection_id = self
            .src_connection_id()
            .ok_or_else(ConnectionError::missing_local_connection_id)?;
        let dst_connection_id = self
            .dst_connection_id()
            .ok_or_else(ConnectionError::missing_counterparty_connection_id)?;

        let _expected_dst_connection =
            self.validated_expected_connection(ConnectionMsgType::OpenAck)?;

        let src_connection = self
            .src_chain()
            .query_connection(src_connection_id, Height::tagged_zero())
            .map_err(|e| ConnectionError::chain_query(self.src_chain().id().untag(), e))?;

        // TODO - check that the src connection is consistent with the ack options

        // Build add **send** the message(s) for updating client on source.
        // TODO - add check if it is required
        let src_client_target_height = self
            .dst_chain()
            .query_latest_height()
            .map_err(|e| ConnectionError::chain_query(self.dst_chain().id().untag(), e))?;
        let client_msgs = self.build_update_client_on_src(src_client_target_height)?;
        self.src_chain()
            .send_msgs(client_msgs)
            .map_err(|e| ConnectionError::submit(self.src_chain().id().untag(), e))?;

        let query_height = self
            .src_chain()
            .query_latest_height()
            .map_err(|e| ConnectionError::chain_query(self.src_chain().id().untag(), e))?;

        let (client_state, proofs) = self
            .src_chain()
            .build_connection_proofs_and_client_state(
                ConnectionMsgType::OpenAck,
                src_connection_id,
                self.src_client_id(),
                query_height,
            )
            .map_err(ConnectionError::connection_proof)?;

        // Build message(s) for updating client on destination
        let mut msgs = self.build_update_client_on_dst(proofs.map(|p| p.height()))?;

        // Get signer
        let signer = self
            .dst_chain()
            .get_signer()
            .map_err(|e| ConnectionError::signer(self.dst_chain().id().untag(), e))?;

        let new_msg = MsgConnectionOpenAck::tagged_new(
            dst_connection_id,
            src_connection_id,
            client_state,
            proofs,
            src_connection.versions()[0].clone(),
            signer,
        );

        msgs.push(new_msg.map_into(Msg::to_any));
        Ok(msgs)
    }

    pub fn build_conn_ack_and_send(&self) -> Result<Tagged<ChainB, IbcEvent>, ConnectionError> {
        let dst_msgs = self.build_conn_ack()?;

        let events = self
            .dst_chain()
            .send_msgs(dst_msgs)
            .map_err(|e| ConnectionError::submit(self.dst_chain().id().untag(), e))?;

        // Find the relevant event for connection ack
        for event in events {
            match event.value() {
                IbcEvent::OpenAckConnection(_) => return Ok(event),
                IbcEvent::ChainError(e) => {
                    return Err(ConnectionError::tx_response(e.clone()));
                }
                _ => {}
            }
        }

        Err(ConnectionError::missing_connection_ack_event())
    }

    /// Attempts to build a MsgConnOpenConfirm.
    pub fn build_conn_confirm(&self) -> Result<Vec<Tagged<ChainB, Any>>, ConnectionError> {
        let src_connection_id = self
            .src_connection_id()
            .ok_or_else(ConnectionError::missing_local_connection_id)?;

        let dst_connection_id = self
            .dst_connection_id()
            .ok_or_else(ConnectionError::missing_counterparty_connection_id)?;

        let _expected_dst_connection =
            self.validated_expected_connection(ConnectionMsgType::OpenAck)?;

        let query_height = self
            .src_chain()
            .query_latest_height()
            .map_err(|e| ConnectionError::chain_query(self.src_chain().id().untag(), e))?;

        let _src_connection = self
            .src_chain()
            .query_connection(src_connection_id, query_height)
            .map_err(|e| ConnectionError::connection_query(src_connection_id.untag(), e))?;

        // TODO - check that the src connection is consistent with the confirm options

        let (_, proofs) = self
            .src_chain()
            .build_connection_proofs_and_client_state(
                ConnectionMsgType::OpenConfirm,
                src_connection_id,
                self.src_client_id(),
                query_height,
            )
            .map_err(ConnectionError::connection_proof)?;

        // Build message(s) for updating client on destination
        let mut msgs = self.build_update_client_on_dst(proofs.map(|p| p.height()))?;

        // Get signer
        let signer = self
            .dst_chain()
            .get_signer()
            .map_err(|e| ConnectionError::signer(self.dst_chain().id().untag(), e))?;

        let new_msg =
            MsgConnectionOpenConfirm::tagged_new(dst_connection_id.clone(), proofs, signer);

        msgs.push(new_msg.map_into(Msg::to_any));
        Ok(msgs)
    }

    pub fn build_conn_confirm_and_send(&self) -> Result<Tagged<ChainB, IbcEvent>, ConnectionError> {
        let dst_msgs = self.build_conn_confirm()?;

        let events = self
            .dst_chain()
            .send_msgs(dst_msgs)
            .map_err(|e| ConnectionError::submit(self.dst_chain().id().untag(), e))?;

        // Find the relevant event for connection confirm
        for event in events {
            match event.value() {
                IbcEvent::OpenConfirmConnection(_) => {
                    return Ok(event);
                }
                IbcEvent::ChainError(e) => {
                    return Err(ConnectionError::tx_response(e.clone()));
                }
                _ => {}
            }
        }

        Err(ConnectionError::missing_connection_confirm_event())
    }

    fn restore_src_client(&self) -> ForeignClient<ChainA, ChainB> {
        ForeignClient::restore(
            self.src_client_id().clone(),
            self.src_chain().clone(),
            self.dst_chain().clone(),
        )
    }

    fn restore_dst_client(&self) -> ForeignClient<ChainB, ChainA> {
        ForeignClient::restore(
            self.dst_client_id().clone(),
            self.dst_chain().clone(),
            self.src_chain().clone(),
        )
    }
}

fn extract_connection_id(event: &IbcEvent) -> Result<ConnectionId, ConnectionError> {
    match event {
        IbcEvent::OpenInitConnection(ev) => ev.connection_id().clone(),
        IbcEvent::OpenTryConnection(ev) => ev.connection_id().clone(),
        IbcEvent::OpenAckConnection(ev) => ev.connection_id().clone(),
        IbcEvent::OpenConfirmConnection(ev) => ev.connection_id().clone(),
        _ => None,
    }
    .ok_or_else(ConnectionError::missing_connection_id_from_event)
}

/// Enumeration of proof carrying ICS3 message, helper for relayer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionMsgType {
    OpenTry,
    OpenAck,
    OpenConfirm,
}

fn check_destination_connection_state<Chain, Counterparty>(
    connection_id: Tagged<Chain, ConnectionId>,
    existing_connection: ConnectionEnd<Chain, Counterparty>,
    expected_connection: ConnectionEnd<Chain, Counterparty>,
) -> Result<(), ConnectionError> {
    let good_client_ids = existing_connection.client_id() == expected_connection.client_id()
        && existing_connection.counterparty().client_id()
            == expected_connection.counterparty().client_id();

    let good_state =
        existing_connection.state().untag() as u32 <= expected_connection.state().untag() as u32;

    let good_connection_ids = existing_connection.counterparty().connection_id().is_none()
        || existing_connection.counterparty().connection_id()
            == expected_connection.counterparty().connection_id();

    // TODO check versions and store prefix

    if good_state && good_client_ids && good_connection_ids {
        Ok(())
    } else {
        Err(ConnectionError::connection_already_exist(
            connection_id.untag(),
        ))
    }
}
