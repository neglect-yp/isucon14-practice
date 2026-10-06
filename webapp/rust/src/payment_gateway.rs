use crate::Error;

#[derive(Debug, thiserror::Error)]
pub enum PaymentGatewayError {
    #[error("reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("payment gateway returned {0}")]
    Status(reqwest::StatusCode),
}

#[derive(Debug, serde::Serialize)]
pub struct PaymentGatewayPostPaymentRequest {
    pub amount: i32,
}

pub async fn request_payment_gateway_post_payment(
    client: &reqwest::Client,
    payment_gateway_url: &str,
    token: &str,
    idempotency_key: &str,
    param: &PaymentGatewayPostPaymentRequest,
) -> Result<(), Error> {
    let mut last = None;
    for attempt in 0..6 {
        match client
            .post(format!("{payment_gateway_url}/payments"))
            .bearer_auth(token)
            .header("Idempotency-Key", idempotency_key)
            .json(param)
            .send()
            .await
        {
            Ok(response) if response.status() == reqwest::StatusCode::NO_CONTENT => return Ok(()),
            Ok(response) => {
                let status = response.status();
                if matches!(status.as_u16(), 400 | 401 | 403 | 422) {
                    return Err(PaymentGatewayError::Status(status).into());
                }
                last = Some(PaymentGatewayError::Status(status));
            }
            Err(error) => last = Some(PaymentGatewayError::Reqwest(error)),
        }
        if attempt < 5 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    Err(last.unwrap().into())
}
