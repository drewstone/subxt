// Copyright 2019-2022 Parity Technologies (UK) Ltd.
// This file is dual-licensed as Apache-2.0 or GPL-3.0.
// see LICENSE for license details.

use crate::{
    client::{
        OfflineClientT,
        OnlineClientT,
    },
    error::{
        BlockError,
        Error,
    },
    events::{
        self,
        Events,
    },
    metadata::DecodeWithMetadata,
    rpc::{
        types::{
            ChainHeadEvent,
            ChainHeadResult,
        },
        ChainBlockResponse,
    },
    storage::{
        address::Yes,
        utils,
        StorageAddress,
    },
    Config,
};
use codec::Decode;
use derivative::Derivative;
use futures::lock::Mutex as AsyncMutex;
use sp_core::twox_128;
use sp_runtime::traits::{
    Hash,
    Header,
};
use std::sync::Arc;

/// A representation of a block obtained from the `chainHead_follow` subscription.
pub struct ChainHeadBlock<T: Config, C> {
    /// The hash of the block.
    hash: T::Hash,
    /// The ID of the subscription that produced this block.
    subscription_id: String,
    /// The client to communicate with the chain.
    client: C,
}

impl<T, C> ChainHeadBlock<T, C>
where
    T: Config,
    C: OfflineClientT<T>,
{
    pub(crate) fn new(hash: T::Hash, subscription_id: String, client: C) -> Self {
        Self {
            hash,
            subscription_id,
            client,
        }
    }

    /// Return the block hash.
    pub fn hash(&self) -> T::Hash {
        self.hash.clone()
    }
}

/// Error resulted from the [`ChainHeadBlock`] methods.
#[derive(Debug, thiserror::Error)]
pub enum ChainHeadError {
    /// The resources requested are inaccessible.
    ///
    /// Resubmitting the request later might succeed.
    #[error("Inaccessible: {0}")]
    Inaccessible(String),
    /// The chain encountered an error. This is definitive.
    #[error("Error: {0}")]
    Error(String),
    /// The provided subscription ID is stale or invalid.
    #[error("Disjoint")]
    Disjoint,
    /// The RPC target node does not contain the given resource.
    #[error("Resource does not exist on the RPC target node")]
    ResourceNonExistent,
    /// An error occurred internally. This is definitive.
    #[error("Other: {0}")]
    Other(String),
}

impl From<Error> for ChainHeadError {
    fn from(error: Error) -> Self {
        ChainHeadError::Other(error.to_string())
    }
}

impl TryFrom<ChainHeadEvent<String>> for Vec<u8> {
    type Error = ChainHeadError;

    fn try_from(event: ChainHeadEvent<String>) -> Result<Self, Self::Error> {
        match event {
            ChainHeadEvent::Done(ChainHeadResult { result }) => {
                let bytes = hex::decode(result.trim_start_matches("0x"))
                    .map_err(|err| ChainHeadError::Other(err.to_string()))?;
                Ok(bytes)
            }
            ChainHeadEvent::Inaccessible(err) => {
                Err(ChainHeadError::Inaccessible(err.error))
            }
            ChainHeadEvent::Error(err) => Err(ChainHeadError::Error(err.error)),
            ChainHeadEvent::Disjoint => Err(ChainHeadError::Disjoint),
        }
    }
}

impl TryFrom<ChainHeadEvent<Option<String>>> for Option<Vec<u8>> {
    type Error = ChainHeadError;

    fn try_from(event: ChainHeadEvent<Option<String>>) -> Result<Self, Self::Error> {
        match event {
            ChainHeadEvent::Done(ChainHeadResult { result }) => {
                let result = match result {
                    Some(result) => result,
                    None => return Ok(None),
                };

                let res: Vec<u8> =
                    ChainHeadEvent::Done(ChainHeadResult { result }).try_into()?;
                Ok(Some(res))
            }
            ChainHeadEvent::Inaccessible(err) => {
                Err(ChainHeadError::Inaccessible(err.error))
            }
            ChainHeadEvent::Error(err) => Err(ChainHeadError::Error(err.error)),
            ChainHeadEvent::Disjoint => Err(ChainHeadError::Disjoint),
        }
    }
}

impl<T, C> ChainHeadBlock<T, C>
where
    T: Config,
    C: OnlineClientT<T>,
{
    /// Fetch the body (vector of extrinsics) of this block.
    pub async fn body(&self) -> Result<Vec<Vec<u8>>, ChainHeadError> {
        self.fetch_body(self.subscription_id.clone(), self.hash)
            .await
    }

    /// Fetch the header of this block.
    pub async fn header(&self) -> Result<T::Header, ChainHeadError> {
        self.fetch_header(self.subscription_id.clone(), self.hash)
            .await
    }

    /// Fetch the raw storage bytes of this block at the provided key.
    pub async fn storage_raw<'a>(
        &self,
        key: &'a [u8],
    ) -> Result<Option<Vec<u8>>, ChainHeadError> {
        self.fetch_storage(self.subscription_id.clone(), self.hash, &key, None)
            .await
    }

    /// Fetch the storage of this block at the provided key.
    pub async fn storage<'a, Address>(
        &self,
        key: &'a Address,
    ) -> Result<Option<<Address::Target as DecodeWithMetadata>::Target>, ChainHeadError>
    where
        Address: StorageAddress<IsFetchable = Yes> + 'a,
    {
        // Look up the return type ID to enable DecodeWithMetadata:
        let metadata = self.client.metadata();
        let key_bytes = utils::storage_address_bytes(key, &metadata)?;

        let storage_bytes = self.storage_raw(&key_bytes).await?;
        let bytes = match storage_bytes {
            Some(bytes) => bytes,
            None => return Ok(None),
        };

        let storage =
            <Address::Target as DecodeWithMetadata>::decode_storage_with_metadata(
                &mut &*bytes,
                key.pallet_name(),
                key.entry_name(),
                &metadata,
            )?;
        Ok(Some(storage))
    }

    /// Execute a runtime API call at this block.
    pub async fn call(
        &self,
        function: String,
        call_parameters: Option<&[u8]>,
    ) -> Result<Vec<u8>, ChainHeadError> {
        self.fetch_call(function, call_parameters).await
    }

    /// Unpin this block.
    ///
    /// # Note
    ///
    /// Call this method when you are no longer interested in making queries
    /// against this block.
    ///
    /// Failing to call this method will eventually terminate the subscription.
    pub async fn unpin(self) -> Result<(), Error> {
        self.client
            .rpc()
            .chainhead_unpin(self.subscription_id, self.hash)
            .await?;
        Ok(())
    }

    /// Fetch the block's events.
    pub async fn events(&self) -> Result<Events<T>, ChainHeadError> {
        let mut storage_key = twox_128(b"System").to_vec();
        storage_key.extend(twox_128(b"Events").to_vec());
        let Some(event_bytes) = self.storage_raw(&storage_key).await? else {
            return Err(ChainHeadError::Other(
                "Failed to fetch System::Events storage".into()
            ))
        };

        Ok(Events::new(
            self.client.metadata(),
            self.hash.clone(),
            event_bytes,
        ))
    }

    /// Wrapper to fetch the block's body from the `chainHead_body` subscription.
    async fn fetch_body(
        &self,
        subscription_id: String,
        hash: T::Hash,
    ) -> Result<Vec<Vec<u8>>, ChainHeadError> {
        let mut sub = self
            .client
            .rpc()
            .subscribe_chainhead_body(subscription_id, hash)
            .await?;

        if let Some(event) = sub.next().await {
            let event = event?;

            let bytes = Vec::<u8>::try_from(event)?;

            let extrinsics: Vec<Vec<u8>> =
                Decode::decode(&mut &bytes[..]).map_err(Into::<Error>::into)?;
            return Ok(extrinsics)
        }

        Err(ChainHeadError::Other(
            "Failed to fetch the block body".into(),
        ))
    }

    /// Wrapper to fetch the block's header from the `chainHead_header` method.
    async fn fetch_header(
        &self,
        subscription_id: String,
        hash: T::Hash,
    ) -> Result<T::Header, ChainHeadError> {
        let header = self
            .client
            .rpc()
            .chainhead_header(subscription_id, hash)
            .await?;

        let header = match header {
            Some(header) => header,
            None => return Err(ChainHeadError::ResourceNonExistent),
        };

        let bytes = hex::decode(header.trim_start_matches("0x"))
            .map_err(|err| Error::Other(err.to_string()))?;

        let header: T::Header =
            Decode::decode(&mut &bytes[..]).map_err(Into::<Error>::into)?;
        Ok(header)
    }

    /// Wrapper to fetch the block's storage from the `chainHead_storage` subscription.
    async fn fetch_storage(
        &self,
        subscription_id: String,
        hash: T::Hash,
        key: &[u8],
        child_key: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, ChainHeadError> {
        let mut sub = self
            .client
            .rpc()
            .subscribe_chainhead_storage(subscription_id, hash, key, child_key)
            .await?;

        if let Some(event) = sub.next().await {
            let event = event?;

            let bytes = Option::<Vec<u8>>::try_from(event)?;
            return Ok(bytes)
        }

        Err(ChainHeadError::Other(
            "Failed to fetch the block storage".into(),
        ))
    }

    /// Execute a runtime API call at this block.
    async fn fetch_call(
        &self,
        function: String,
        call_parameters: Option<&[u8]>,
    ) -> Result<Vec<u8>, ChainHeadError> {
        let call_parameters = call_parameters.unwrap_or(Default::default());

        let mut sub = self
            .client
            .rpc()
            .subscribe_chainhead_call(
                self.subscription_id.clone(),
                self.hash,
                function,
                call_parameters,
            )
            .await?;

        if let Some(event) = sub.next().await {
            let event = event?;

            let bytes = Vec::<u8>::try_from(event)?;
            return Ok(bytes)
        }

        Err(ChainHeadError::Other(
            "Failed to execute the runtime API call".into(),
        ))
    }
}

/// A representation of a block.
pub struct Block<T: Config, C> {
    header: T::Header,
    client: C,
    // Since we obtain the same events for every extrinsic, let's
    // cache them so that we only ever do that once:
    cached_events: CachedEvents<T>,
}

// A cache for our events so we don't fetch them more than once when
// iterating over events for extrinsics.
type CachedEvents<T> = Arc<AsyncMutex<Option<events::Events<T>>>>;

impl<T, C> Block<T, C>
where
    T: Config,
    C: OfflineClientT<T>,
{
    pub(crate) fn new(header: T::Header, client: C) -> Self {
        Block {
            header,
            client,
            cached_events: Default::default(),
        }
    }

    /// Return the block hash.
    pub fn hash(&self) -> T::Hash {
        self.header.hash()
    }

    /// Return the block number.
    pub fn number(&self) -> T::BlockNumber {
        *self.header().number()
    }

    /// Return the entire block header.
    pub fn header(&self) -> &T::Header {
        &self.header
    }
}

impl<T, C> Block<T, C>
where
    T: Config,
    C: OnlineClientT<T>,
{
    /// Return the events associated with the block, fetching them from the node if necessary.
    pub async fn events(&self) -> Result<events::Events<T>, Error> {
        get_events(&self.client, self.header.hash(), &self.cached_events).await
    }

    /// Fetch and return the block body.
    pub async fn body(&self) -> Result<BlockBody<T, C>, Error> {
        let block_hash = self.header.hash();
        let block_details = match self.client.rpc().block(Some(block_hash)).await? {
            Some(block) => block,
            None => return Err(BlockError::block_hash_not_found(block_hash).into()),
        };

        Ok(BlockBody::new(
            self.client.clone(),
            block_details,
            self.cached_events.clone(),
        ))
    }
}

/// The body of a block.
pub struct BlockBody<T: Config, C> {
    details: ChainBlockResponse<T>,
    client: C,
    cached_events: CachedEvents<T>,
}

impl<T, C> BlockBody<T, C>
where
    T: Config,
    C: OfflineClientT<T>,
{
    pub(crate) fn new(
        client: C,
        details: ChainBlockResponse<T>,
        cached_events: CachedEvents<T>,
    ) -> Self {
        Self {
            details,
            client,
            cached_events,
        }
    }

    /// Returns an iterator over the extrinsics in the block body.
    pub fn extrinsics(&self) -> impl Iterator<Item = Extrinsic<'_, T, C>> {
        self.details
            .block
            .extrinsics
            .iter()
            .enumerate()
            .map(|(idx, e)| {
                Extrinsic {
                    index: idx as u32,
                    bytes: &e.0,
                    client: self.client.clone(),
                    block_hash: self.details.block.header.hash(),
                    cached_events: self.cached_events.clone(),
                    _marker: std::marker::PhantomData,
                }
            })
    }
}

/// A single extrinsic in a block.
pub struct Extrinsic<'a, T: Config, C> {
    index: u32,
    bytes: &'a [u8],
    client: C,
    block_hash: T::Hash,
    cached_events: CachedEvents<T>,
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T, C> Extrinsic<'a, T, C>
where
    T: Config,
    C: OfflineClientT<T>,
{
    /// The index of the extrinsic in the block.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// The bytes of the extrinsic.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

impl<'a, T, C> Extrinsic<'a, T, C>
where
    T: Config,
    C: OnlineClientT<T>,
{
    /// The events associated with the extrinsic.
    pub async fn events(&self) -> Result<ExtrinsicEvents<T>, Error> {
        let events =
            get_events(&self.client, self.block_hash, &self.cached_events).await?;
        let ext_hash = T::Hashing::hash_of(&self.bytes);
        Ok(ExtrinsicEvents::new(ext_hash, self.index, events))
    }
}

/// The events associated with a given extrinsic.
#[derive(Derivative)]
#[derivative(Debug(bound = ""))]
pub struct ExtrinsicEvents<T: Config> {
    // The hash of the extrinsic (handy to expose here because
    // this type is returned from TxProgress things in the most
    // basic flows, so it's the only place people can access it
    // without complicating things for themselves).
    ext_hash: T::Hash,
    // The index of the extrinsic:
    idx: u32,
    // All of the events in the block:
    events: events::Events<T>,
}

impl<T: Config> ExtrinsicEvents<T> {
    pub(crate) fn new(ext_hash: T::Hash, idx: u32, events: events::Events<T>) -> Self {
        Self {
            ext_hash,
            idx,
            events,
        }
    }

    /// Return the hash of the block that the extrinsic is in.
    pub fn block_hash(&self) -> T::Hash {
        self.events.block_hash()
    }

    /// The index of the extrinsic that these events are produced from.
    pub fn extrinsic_index(&self) -> u32 {
        self.idx
    }

    /// Return the hash of the extrinsic.
    pub fn extrinsic_hash(&self) -> T::Hash {
        self.ext_hash
    }

    /// Return all of the events in the block that the extrinsic is in.
    pub fn all_events_in_block(&self) -> &events::Events<T> {
        &self.events
    }

    /// Iterate over all of the raw events associated with this transaction.
    ///
    /// This works in the same way that [`events::Events::iter()`] does, with the
    /// exception that it filters out events not related to the submitted extrinsic.
    pub fn iter(&self) -> impl Iterator<Item = Result<events::EventDetails, Error>> + '_ {
        self.events.iter().filter(|ev| {
            ev.as_ref()
                .map(|ev| ev.phase() == events::Phase::ApplyExtrinsic(self.idx))
                .unwrap_or(true) // Keep any errors.
        })
    }

    /// Find all of the transaction events matching the event type provided as a generic parameter.
    ///
    /// This works in the same way that [`events::Events::find()`] does, with the
    /// exception that it filters out events not related to the submitted extrinsic.
    pub fn find<Ev: events::StaticEvent>(
        &self,
    ) -> impl Iterator<Item = Result<Ev, Error>> + '_ {
        self.iter().filter_map(|ev| {
            ev.and_then(|ev| ev.as_event::<Ev>().map_err(Into::into))
                .transpose()
        })
    }

    /// Iterate through the transaction events using metadata to dynamically decode and skip
    /// them, and return the first event found which decodes to the provided `Ev` type.
    ///
    /// This works in the same way that [`events::Events::find_first()`] does, with the
    /// exception that it ignores events not related to the submitted extrinsic.
    pub fn find_first<Ev: events::StaticEvent>(&self) -> Result<Option<Ev>, Error> {
        self.find::<Ev>().next().transpose()
    }

    /// Find an event in those associated with this transaction. Returns true if it was found.
    ///
    /// This works in the same way that [`events::Events::has()`] does, with the
    /// exception that it ignores events not related to the submitted extrinsic.
    pub fn has<Ev: events::StaticEvent>(&self) -> Result<bool, Error> {
        Ok(self.find::<Ev>().next().transpose()?.is_some())
    }
}

// Return Events from the cache, or fetch from the node if needed.
async fn get_events<C, T>(
    client: &C,
    block_hash: T::Hash,
    cached_events: &AsyncMutex<Option<events::Events<T>>>,
) -> Result<events::Events<T>, Error>
where
    T: Config,
    C: OnlineClientT<T>,
{
    // Acquire lock on the events cache. We either get back our events or we fetch and set them
    // before unlocking, so only one fetch call should ever be made. We do this because the
    // same events can be shared across all extrinsics in the block.
    let lock = cached_events.lock().await;
    let events = match &*lock {
        Some(events) => events.clone(),
        None => {
            events::EventsClient::new(client.clone())
                .at(Some(block_hash))
                .await?
        }
    };

    Ok(events)
}
