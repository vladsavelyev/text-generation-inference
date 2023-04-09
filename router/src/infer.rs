/// Batching and inference logic
use crate::validation::{Validation, ValidationError};
use crate::GenerateRequest;
use crate::{Db, Entry, Token};
use nohash_hasher::IntMap;
use std::future::Future;
use std::sync::Arc;
use text_generation_client::{
    Batch, ClientError, GeneratedText, Generation, PrefillTokens, ShardedClient,
};
use thiserror::Error;
use tokio::sync::{mpsc, Notify, Semaphore, TryAcquireError};
use tokio::time::Instant;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;
use tracing::instrument;

/// Inference struct
#[derive(Clone)]
pub struct Infer {
    /// Validation
    validation: Validation,
    /// Request database
    db: Db,
    /// Shared state
    shared: Arc<Shared>,
    /// Inference limit
    limit_concurrent_requests: Arc<Semaphore>,
}

/// Infer shared state
struct Shared {
    /// Batching background Tokio task notifier
    batching_task: Notify,
}

impl Infer {
    pub(crate) fn new(
        client: ShardedClient,
        validation: Validation,
        max_batch_size: usize,
        max_waiting_tokens: usize,
        max_concurrent_requests: usize,
    ) -> Self {
        // Infer shared state
        let db = Db::new();
        let shared = Arc::new(Shared {
            batching_task: Notify::new(),
        });

        // Spawn batching background task that contains all the inference logic
        tokio::spawn(batching_task(
            client,
            max_batch_size,
            max_waiting_tokens,
            db.clone(),
            shared.clone(),
        ));

        // Inference limit with a semaphore
        let semaphore = Arc::new(Semaphore::new(max_concurrent_requests));

        Self {
            validation,
            db,
            shared,
            limit_concurrent_requests: semaphore,
        }
    }

    /// Add a new request to the database and return a stream of InferStreamResponse
    pub(crate) async fn generate_stream(
        &self,
        request: GenerateRequest,
    ) -> Result<UnboundedReceiverStream<Result<InferStreamResponse, InferError>>, InferError> {
        // Limit concurrent requests by acquiring a permit from the semaphore
        // This permit will live as long as Entry
        let permit = self.clone().limit_concurrent_requests.try_acquire_owned()?;

        // Validate request
        let (input_length, validated_request) = self.validation.validate(request).await?;

        // MPSC channel to communicate with the background batching task
        let (response_tx, response_rx) = mpsc::unbounded_channel();

        // Append the request to the database
        self.db.append(Entry {
            request: validated_request,
            response_tx,
            input_length,
            time: Instant::now(),
            batch_time: None,
            _permit: permit,
        });

        // Notify the background task that we have a new entry in the database that needs
        // to be batched
        self.shared.batching_task.notify_one();

        // Return stream
        Ok(UnboundedReceiverStream::new(response_rx))
    }

    /// Add a new request to the database and return a InferResponse
    pub(crate) async fn generate(
        &self,
        request: GenerateRequest,
    ) -> Result<InferResponse, InferError> {
        // Create stream
        let mut stream = self.generate_stream(request).await?;

        // Return values
        let mut result_prefill = Vec::new();
        let mut result_tokens = Vec::new();
        let mut result_generated_text = None;
        let mut result_start = None;
        let mut result_queued = None;

        // Iterate on stream
        while let Some(response) = stream.next().await {
            match response? {
                // Add prefill tokens
                InferStreamResponse::Prefill(tokens) => {
                    // Create Token objects
                    // We do that here instead of in the Python code as Rust for loops are faster
                    result_prefill = tokens
                        .ids
                        .into_iter()
                        .zip(tokens.logprobs.into_iter())
                        .zip(tokens.texts.into_iter())
                        .map(|((id, logprob), text)| Token(id, text, logprob))
                        .collect();
                }
                // Push last token
                InferStreamResponse::Token(token) => result_tokens.push(token),
                // Final message
                // Set return values
                InferStreamResponse::End {
                    token,
                    generated_text,
                    start,
                    queued,
                } => {
                    result_tokens.push(token);
                    result_generated_text = Some(generated_text);
                    result_start = Some(start);
                    result_queued = Some(queued)
                }
            }
        }

        // Check that we received a `InferStreamResponse::End` message
        if let (Some(generated_text), Some(queued), Some(start)) =
            (result_generated_text, result_queued, result_start)
        {
            Ok(InferResponse {
                prefill: result_prefill,
                tokens: result_tokens,
                generated_text,
                queued,
                start,
            })
        } else {
            Err(InferError::IncompleteGeneration)
        }
    }
}

/// Batching logic
/// Will be launched in a background Tokio task
///
/// Batches requests and sends them to the inference server
#[instrument(skip(client, db, shared))]
async fn batching_task(
    mut client: ShardedClient,
    max_batch_size: usize,
    max_waiting_tokens: usize,
    db: Db,
    shared: Arc<Shared>,
) {
    // Minimum batch size after which we try to add more requests
    let limit_min_batch_size = (max_batch_size / 2) as u32;

    // Infinite loop
    loop {
        // Wait for a notification from the Infer struct
        shared.batching_task.notified().await;

        // Get the next batch from the DB
        // This batch might be smaller than the maximum batch size if there are not enough requests
        // waiting in the DB
        while let Some((mut entries, batch)) = db.next_batch(None, max_batch_size) {
            let mut cached_batch = wrap_future(client.prefill(batch), &mut entries).await;
            let mut waiting_tokens = 1;

            // We loop until we do not receive any cached batch from the inference server (== until
            // all requests have met their stopping criteria)
            while let Some(batch) = cached_batch {
                // Get current batch info
                let batch_size = batch.size;
                let mut batches = vec![batch];

                // If the current batch is too small, we try to add more requests to it
                if batch_size <= limit_min_batch_size {
                    let min_size = match waiting_tokens {
                        // If we didn't onboard any new requests since >= max_waiting_tokens, we try
                        // to add a new batch even though its size might be small
                        _ if waiting_tokens >= max_waiting_tokens => None,
                        // Minimum size criteria
                        _ => Some(limit_min_batch_size as usize),
                    };

                    // Try to get a new batch
                    if let Some((mut new_entries, new_batch)) =
                        db.next_batch(min_size, max_batch_size - batch_size as usize)
                    {
                        // Generate one token for this new batch to have the attention past in cache
                        let new_cached_batch =
                            wrap_future(client.prefill(new_batch), &mut new_entries).await;
                        // Reset waiting counter
                        waiting_tokens = 1;
                        // Extend current batch with the new batch
                        if let Some(new_cached_batch) = new_cached_batch {
                            entries.extend(new_entries);
                            batches.push(new_cached_batch);
                        }
                    }
                }

                cached_batch = wrap_future(client.decode(batches), &mut entries).await;
                waiting_tokens += 1;
            }
        }
    }
}

/// Wrap a future inside a match statement to handle errors and send the responses to Infer
async fn wrap_future(
    future: impl Future<Output = Result<(Vec<Generation>, Option<Batch>), ClientError>>,
    entries: &mut IntMap<u64, Entry>,
) -> Option<Batch> {
    match future.await {
        Ok((generations, next_batch)) => {
            send_generations(generations, entries);
            next_batch
        }
        // If we have an error, we discard the whole batch
        Err(err) => {
            send_error(err, entries);
            None
        }
    }
}

/// Send errors to Infer for all `entries`
fn send_error(error: ClientError, entries: &mut IntMap<u64, Entry>) {
    entries.drain().for_each(|(_, entry)| {
        // unwrap_or is valid here as we don't care if the receiver is gone.
        entry
            .response_tx
            .send(Err(InferError::GenerationError(error.to_string())))
            .unwrap_or(());
    });
}

/// Send one or multiple `InferStreamResponse` to Infer for all `entries`
fn send_generations(generations: Vec<Generation>, entries: &mut IntMap<u64, Entry>) {
    generations.into_iter().for_each(|generation| {
        // Get entry
        // We can `expect` here as the request id should always be in the entries
        let entry = entries
            .get(&generation.request_id)
            .expect("ID not found in entries. This is a bug.");

        if let Some(prefill_tokens) = generation.prefill_tokens {
            // Send message
            // unwrap_or is valid here as we don't care if the receiver is gone.
            entry
                .response_tx
                .send(Ok(InferStreamResponse::Prefill(prefill_tokens)))
                .unwrap_or(());
        }

        // Create last Token
        let token = Token(
            generation.token_id,
            generation.token_text,
            generation.token_logprob,
        );

        if let Some(generated_text) = generation.generated_text {
            // Remove entry as this is the last message
            // We can `expect` here as the request id should always be in the entries
            let entry = entries
                .remove(&generation.request_id)
                .expect("ID not found in entries. This is a bug.");

            // Send message
            // unwrap_or is valid here as we don't care if the receiver is gone.
            entry
                .response_tx
                .send(Ok(InferStreamResponse::End {
                    token,
                    generated_text,
                    queued: entry.time,
                    start: entry.batch_time.unwrap(),
                }))
                .unwrap_or(());
        } else {
            // Send message
            // unwrap_or is valid here as we don't care if the receiver is gone.
            entry
                .response_tx
                .send(Ok(InferStreamResponse::Token(token)))
                .unwrap_or(());
        }
    });
}

#[derive(Debug)]
pub(crate) enum InferStreamResponse {
    // Optional first message
    Prefill(PrefillTokens),
    // Intermediate messages
    Token(Token),
    // Last message
    End {
        token: Token,
        generated_text: GeneratedText,
        start: Instant,
        queued: Instant,
    },
}

#[derive(Debug)]
pub(crate) struct InferResponse {
    pub(crate) prefill: Vec<Token>,
    pub(crate) tokens: Vec<Token>,
    pub(crate) generated_text: GeneratedText,
    pub(crate) queued: Instant,
    pub(crate) start: Instant,
}

#[derive(Debug, Error)]
pub enum InferError {
    #[error("Request failed during generation: {0}")]
    GenerationError(String),
    #[error("Model is overloaded")]
    Overloaded(#[from] TryAcquireError),
    #[error("Input validation error: {0}")]
    ValidationError(#[from] ValidationError),
    #[error("Incomplete generation")]
    IncompleteGeneration,
}
