use std::convert::Infallible;
use crate::{
    Batcher, Details, ErrorResponse, GenerateParameters, GenerateRequest, GeneratedText, Validation
};
use axum::extract::Extension;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Result, Sse};
use axum::response::sse::{Event, KeepAlive};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use futures::stream::unfold;
use text_generation_client::{ShardedClient};
use tokenizers::Tokenizer;
use tokio::signal;
use tokio::sync::{Semaphore};
use tokio::time::{Instant, sleep};
use tokio_stream::{Stream};
use tracing::instrument;

// Server shared state
#[derive(Clone)]
struct ServerState {
    validation: Validation,
    batcher: Batcher,
    limit_concurrent_requests: Arc<Semaphore>,
}

/// Health check method
#[instrument(skip(state), fields(time, time_per_token))]
async fn health(state: Extension<ServerState>) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    // TODO: while this is the best health check we can do, it is a bit on the heavy side and might
    //       be a bit too slow for a health check.
    //       What we should do instead if check if the gRPC channels are still healthy.

    // Limit concurrent requests by acquiring a permit from the semaphore
    let _permit = state.limit_concurrent_requests.try_acquire().map_err(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse {
                error: "Model is overloaded".to_string(),
            }),
        )
    })?;

    // Send a small inference request
    state
        .batcher
        .infer(
            1,
            GenerateRequest {
                inputs: "liveness".to_string(),
                parameters: GenerateParameters {
                    temperature: 1.0,
                    top_k: 0,
                    top_p: 1.0,
                    do_sample: false,
                    max_new_tokens: 1,
                    stop: vec![],
                    details: false,
                    seed: None,
                },
            },
        )
        .await?;
    Ok(())
}

/// Generate method
#[instrument(
    skip(state),
    fields(
        total_time,
        validation_time,
        queue_time,
        inference_time,
        time_per_token,
        seed
    )
)]
async fn generate(
    state: Extension<ServerState>,
    req: Json<GenerateRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let start_time = Instant::now();
    // Limit concurrent requests by acquiring a permit from the semaphore
    let _permit = state.limit_concurrent_requests.try_acquire().map_err(|_| {
        tracing::error!("Model is overloaded");
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse {
                error: "Model is overloaded".to_string(),
            }),
        )
    })?;

    // Validate request
    let details = req.0.parameters.details;
    let (input_length, validated_request) =
        state.validation.validate(req.0).await.map_err(|err| {
            tracing::error!("{}", err.to_string());
            err
        })?;

    // Inference
    let response = state
        .batcher
        .infer(input_length, validated_request)
        .await
        .map_err(|err| {
            tracing::error!("{}", err.to_string());
            err
        })?;

    // Token details
    let details = match details {
        true => {
            let tokens = response
                .token_ids
                .into_iter()
                .zip(response.tokens.into_iter())
                .zip(response.logprobs.into_iter())
                .map(|((id, text), logprob)| (id, text, logprob))
                .collect();
            Some(Details {
                seed: response.seed,
                finish_reason: response.finish_reason,
                generated_tokens: response.generated_tokens,
                tokens,
            })
        }
        false => None,
    };

    // Timings
    let total_time = start_time.elapsed();
    let validation_time = response.queued - start_time;
    let queue_time = response.start - response.queued;
    let inference_time = response.end - response.start;
    let time_per_token = inference_time / response.generated_tokens;

    // Headers
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-total-time",
        total_time.as_millis().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-validation-time",
        validation_time.as_millis().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-queue-time",
        queue_time.as_millis().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-inference-time",
        inference_time.as_millis().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-time-per-token",
        time_per_token.as_millis().to_string().parse().unwrap(),
    );

    // Tracing metadata
    tracing::Span::current().record("total_time", format!("{:?}", total_time));
    tracing::Span::current().record("validation_time", format!("{:?}", validation_time));
    tracing::Span::current().record("queue_time", format!("{:?}", queue_time));
    tracing::Span::current().record("inference_time", format!("{:?}", inference_time));
    tracing::Span::current().record("time_per_token", format!("{:?}", time_per_token));
    tracing::Span::current().record("seed", format!("{:?}", response.seed));
    tracing::info!("Output: {}", response.output_text);

    // Send response
    let response = vec![GeneratedText {
        generated_text: response.output_text,
        details,
    }];
    Ok((headers, Json(response)))
}

async fn generate_stream(
    state: Extension<ServerState>,
    req: Json<GenerateRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Limit concurrent requests by acquiring a permit from the semaphore
    let _permit = state.limit_concurrent_requests.try_acquire().map_err(|_| {
        tracing::error!("Model is overloaded");
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse {
                error: "Model is overloaded".to_string(),
            }),
        )
    }).expect("Model is overloaded!");

    // Validate request
    let (input_length, validated_request) =
        state.validation.validate(req.0).await.map_err(|err| {
            tracing::error!("{}", err.to_string());
            err
        }).expect("Validation error!");

    let mut intermediate_rx = state.batcher.infer_stream(
        input_length,
        validated_request,
    );
    
    let stream = async_stream::stream! {
        while let Some(item) = intermediate_rx.recv().await {
            sleep(Duration::from_millis(500)).await;
            
            let response = item.expect("Inference error!");
            if response.is_finished {
                // If the response is finished, return None to stop the stream,
                break
            }
            let event = Event::default().data(response.token);
            yield Ok(event)
        }
    };

    // Create an SSE response with a keep-alive.
    Sse::new(stream).keep_alive(KeepAlive::default())
}


/// Serving method
#[allow(clippy::too_many_arguments)]
pub async fn run(
    max_concurrent_requests: usize,
    max_input_length: usize,
    max_batch_size: usize,
    max_waiting_tokens: usize,
    client: ShardedClient,
    tokenizer: Tokenizer,
    validation_workers: usize,
    addr: SocketAddr,
) {
    // Create state
    let batcher = Batcher::new(client, max_batch_size, max_waiting_tokens);
    let validation = Validation::new(validation_workers, tokenizer, max_input_length);
    let shared_state = ServerState {
        validation,
        batcher,
        limit_concurrent_requests: Arc::new(Semaphore::new(max_concurrent_requests)),
    };

    // Create router
    let app = Router::new()
        .route("/", post(generate))
        .route("/generate", post(generate))
        .route("/generate_stream", post(generate_stream))
        .route("/", get(health))
        .route("/health", get(health))
        .layer(Extension(shared_state.clone()));

    // Run server
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        // Wait until all requests are finished to shut down
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

/// Shutdown signal handler
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("signal received, starting graceful shutdown");
}
