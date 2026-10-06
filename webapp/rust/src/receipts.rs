use crate::{coordinates::CoordinateWriter, notifications::Audience, Error};
use sqlx::{MySql, MySqlPool, QueryBuilder};
use tokio::sync::{mpsc, oneshot};

struct Receipt {
    id: String,
    chair: bool,
    generation: u64,
    reply: oneshot::Sender<Result<(), String>>,
}

#[derive(Debug, Clone)]
pub struct ReceiptWriter {
    sender: mpsc::Sender<Receipt>,
    coordinates: CoordinateWriter,
}

impl ReceiptWriter {
    pub fn new(pool: MySqlPool, coordinates: CoordinateWriter) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Receipt>(1024);
        let lifecycle = coordinates.clone();
        tokio::spawn(async move {
            while let Some(first) = receiver.recv().await {
                let mut batch = vec![first];
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1);
                while batch.len() < 128 {
                    let Ok(Some(next)) = tokio::time::timeout_at(deadline, receiver.recv()).await
                    else {
                        break;
                    };
                    batch.push(next);
                }
                let _guard = lifecycle.initialization.read().await;
                let generation = lifecycle.generation();
                batch = batch
                    .into_iter()
                    .filter_map(|item| {
                        if item.generation == generation {
                            Some(item)
                        } else {
                            let _ = item.reply.send(Err("database was initialized".into()));
                            None
                        }
                    })
                    .collect();
                if batch.is_empty() {
                    continue;
                }
                match persist(&pool, &batch).await {
                    Ok(()) => {
                        for item in batch {
                            let _ = item.reply.send(Ok(()));
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "notification receipt batch failed");
                        for item in batch {
                            let _ = item.reply.send(Err(error.to_string()));
                        }
                    }
                }
            }
        });
        Self {
            sender,
            coordinates,
        }
    }

    pub async fn acknowledge(&self, audience: &Audience, id: String) -> Result<(), Error> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(Receipt {
                id,
                chair: matches!(audience, Audience::Chair(_)),
                generation: self.coordinates.generation(),
                reply,
            })
            .await
            .map_err(|_| Error::Background("receipt writer stopped".into()))?;
        response
            .await
            .map_err(|_| Error::Background("receipt writer stopped".into()))?
            .map_err(Error::Background)
    }
}

async fn persist(pool: &MySqlPool, batch: &[Receipt]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    for (chair, column) in [(false, "app_sent_at"), (true, "chair_sent_at")] {
        let mut ids: Vec<_> = batch
            .iter()
            .filter(|item| item.chair == chair)
            .map(|item| &item.id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            continue;
        }
        let mut query = QueryBuilder::<MySql>::new(format!("UPDATE ride_statuses SET {column}=CURRENT_TIMESTAMP(6) WHERE {column} IS NULL AND id IN ("));
        {
            let mut values = query.separated(",");
            for id in ids {
                values.push_bind(id);
            }
        }
        query.push(")");
        query.build().execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
