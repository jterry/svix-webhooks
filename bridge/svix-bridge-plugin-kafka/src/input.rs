use std::{
    str,
    time::{Duration, Instant},
};

use rdkafka::{
    consumer::{CommitMode, Consumer as _},
    error::KafkaError,
    Message as _,
};
use svix_bridge_types::{
    async_trait, svix::api::Svix, CreateMessageRequest, JsObject, SenderInput, SenderOutputOpts,
    TransformationConfig, TransformerInput, TransformerInputFormat, TransformerJob,
    TransformerOutput, TransformerTx,
};
use tokio::task::spawn_blocking;

use crate::{config::KafkaInputOpts, Error, Result};

pub struct KafkaConsumer {
    name: String,
    opts: KafkaInputOpts,
    transformation: Option<TransformationConfig>,
    transformer_tx: Option<TransformerTx>,
    svix_client: Svix,
}

impl KafkaConsumer {
    pub fn new(
        name: String,
        opts: KafkaInputOpts,
        transformation: Option<TransformationConfig>,
        output: SenderOutputOpts,
    ) -> Result<Self> {
        Ok(Self {
            name,
            transformation,
            transformer_tx: None,
            opts,
            svix_client: match output {
                SenderOutputOpts::Svix(output) => {
                    Svix::new(output.token, output.options.map(Into::into))
                }
            },
        })
    }

    #[tracing::instrument(skip_all)]
    async fn process(&self, msg: &rdkafka::message::BorrowedMessage<'_>) -> Result<()> {
        let payload = msg.payload().ok_or_else(|| Error::MissingPayload)?;
        let payload = if let Some(transformation) = &self.transformation {
            let input = match transformation.format() {
                TransformerInputFormat::Json => {
                    let json_payload =
                        serde_json::from_slice(payload).map_err(Error::Deserialization)?;
                    TransformerInput::JSON(json_payload)
                }
                TransformerInputFormat::String => {
                    let raw_payload = str::from_utf8(payload).map_err(Error::NonUtf8Payload)?;
                    TransformerInput::String(raw_payload.to_string())
                }
            };

            let script = transformation.source().clone();
            let object = self.transform(script, input).await?;
            serde_json::from_value(serde_json::Value::Object(object))
                .map_err(Error::Deserialization)?
        } else {
            serde_json::from_slice(payload).map_err(Error::Deserialization)?
        };

        let CreateMessageRequest {
            app_id,
            message,
            mut post_options,
        } = payload;

        // If committing the message fails or the process crashes after posting the webhook but
        // before committing, this makes sure that the next run of this fn with the same kafka
        // message doesn't end up creating a duplicate webhook in svix.
        let idempotency_key = format!(
            "svix_bridge_kafka_{}_{}_{}",
            self.opts.group_id,
            self.opts.topic,
            msg.offset()
        );
        post_options
            .get_or_insert_with(Default::default)
            .idempotency_key = Some(idempotency_key);

        self.svix_client
            .message()
            .create(app_id, message, post_options.map(Into::into))
            .await?;

        Ok(())
    }

    async fn transform(&self, script: String, input: TransformerInput) -> Result<JsObject> {
        let (job, rx) = TransformerJob::new(script, input);
        self.transformer_tx
            .as_ref()
            .ok_or_else(|| Error::transformation("transformations not configured"))?
            .send(job)
            .map_err(|e| Error::transformation(e.to_string()))?;

        let ret = rx
            .await
            .map_err(|_e| Error::transformation("transformation rx failed"))
            .and_then(|x| {
                x.map_err(|_e| Error::transformation("transformation execution failed"))
            })?;

        match ret {
            TransformerOutput::Object(v) => Ok(v),
            TransformerOutput::Invalid => Err(Error::transformation(
                "transformation produced unexpected value",
            )),
        }
    }

    async fn run_inner(&self) -> Result<()> {
        let opts = self.opts.clone();
        // `ClientConfig::create` does blocking I/O.
        // Same for subscribe, most likely.
        let consumer = spawn_blocking(move || {
            let topic = opts.topic.clone();

            let consumer = opts.create_consumer()?;
            tracing::debug!("Created StreamConsumer");

            consumer.subscribe(&[&topic])?;
            tracing::debug!(topic, "Subscribed");

            Ok::<_, KafkaError>(consumer)
        })
        .await
        .expect("create_consumer task panicked")?;

        loop {
            // FIXME(jplatte): I don't know if StreamConsumer::recv has an internal buffer.
            // Overall, rdkafka seems to be doing a bunch of background magic so maybe it does.
            // In that case, it's likely already doing batching reads (which don't seem to be
            // a thing in the public API) internally.
            // If not, we should likely do some sort of batching ourselves, e.g. have two separate
            // tokio tasks, one which pulls messages from Kafka and one that processes them, with
            // a bounded channel in between for backpressure.
            let msg = consumer.recv().await?;
            tracing::debug!("Received a message");

            let mut process_error_count = 0;
            while let Err(e) = self.process(&msg).await {
                match e {
                    // If the payload is invalid, log an error and continue.
                    // It would fail the same way if retried.
                    Error::MissingPayload
                    | Error::Deserialization(_)
                    | Error::NonUtf8Payload(_) => {
                        tracing::error!(error = &e as &dyn std::error::Error, "invalid payload");
                        break;
                    }

                    // If the error is (possibly) transient, retry a few times.
                    // After that, bubble up the error so it's logged at error level.
                    Error::Kafka(_) | Error::SvixClient(_) | Error::Transformation { .. } => {
                        process_error_count += 1;
                        if process_error_count >= 3 {
                            return Err(e);
                        }

                        tracing::warn!(
                            error = &e as &dyn std::error::Error,
                            "failed to process payload from kafka"
                        );

                        // retry
                    }
                }
            }

            // FIXME(jplatte): Unlike recv above, this seems less likely to be auto-coalesced
            // internally in rdkafka so maybe we should introduce our own logic to only commit
            // after N messages to reduce unnecessary back and forth on the Kafka connection,
            // or unnecessary disk writes inside Kafka (messages in Kafka are not committed
            // individually, rather what this call does is update the stored stream position
            // for the consumer group).
            consumer.commit_message(&msg, CommitMode::Async)?;
        }
    }
}

#[async_trait]
impl SenderInput for KafkaConsumer {
    fn name(&self) -> &str {
        &self.name
    }

    fn set_transformer(&mut self, tx: Option<TransformerTx>) {
        self.transformer_tx = tx;
    }

    async fn run(&self) -> std::io::Result<()> {
        let mut fails: u64 = 0;
        let mut last_fail = Instant::now();

        tracing::info!(topic = self.opts.topic, "Starting to listen for messages");

        loop {
            if let Err(e) = self.run_inner().await {
                tracing::error!("{e}");
            }

            if last_fail.elapsed() > Duration::from_secs(10) {
                // reset the fail count if we didn't have a hiccup in the past short while.
                tracing::trace!("been a while since last fail, resetting count");
                fails = 0;
            } else {
                fails += 1;
            }

            last_fail = Instant::now();
            tokio::time::sleep(Duration::from_millis((300 * fails).min(3000))).await;
        }
    }
}