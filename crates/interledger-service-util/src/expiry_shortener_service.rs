use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use interledger_service::{Account, IlpResult, OutgoingRequest, OutgoingService};
use tracing::trace;

pub const DEFAULT_ROUND_TRIP_TIME: u32 = 500;
pub const DEFAULT_MAX_EXPIRY_DURATION: u32 = 30000;

/// An account with a round trip time, used by the [`ExpiryShortenerService`](./struct.ExpiryShortenerService.html)
/// to shorten a packet's expiration time to account for latency
pub trait RoundTripTimeAccount: Account {
    /// The account's round trip time
    fn round_trip_time(&self) -> u32 {
        DEFAULT_ROUND_TRIP_TIME
    }
}

/// # Expiry Shortener Service
///
/// Each node shortens the `Prepare` packet's expiry duration before passing it on.
/// Nodes shorten the expiry duration so that even if the packet is fulfilled just before the expiry,
/// they will still have enough time to pass the fulfillment to the previous node before it expires.
///
/// This service reduces the expiry time of each packet before forwarding it out.
/// Requires a `RoundtripTimeAccount` and _no store_
#[derive(Clone)]
pub struct ExpiryShortenerService<O> {
    next: O,
    max_expiry_duration: u32,
}

impl<O> ExpiryShortenerService<O> {
    pub fn new(next: O) -> Self {
        ExpiryShortenerService {
            next,
            max_expiry_duration: DEFAULT_MAX_EXPIRY_DURATION,
        }
    }

    // TODO: This isn't used anywhere, should we remove it?
    /// Sets the service's max expiry duration
    pub fn max_expiry_duration(&mut self, milliseconds: u32) -> &mut Self {
        self.max_expiry_duration = milliseconds;
        self
    }
}

#[async_trait]
impl<O, A> OutgoingService<A> for ExpiryShortenerService<O>
where
    O: OutgoingService<A> + Send + Sync + 'static,
    A: RoundTripTimeAccount + Send + Sync + 'static,
{
    /// On send request:
    /// 1. Get the sender and receiver's roundtrip time (default 1000ms)
    /// 2. Reduce the packet's expiry by that amount
    /// 3. Ensure that the packet expiry does not exceed the maximum expiry duration
    /// 4. Forward the request
    async fn send_request(&mut self, mut request: OutgoingRequest<A>) -> IlpResult {
        let time_to_subtract =
            i64::from(request.from.round_trip_time() + request.to.round_trip_time());
        let new_expiry = DateTime::<Utc>::from(request.prepare.expires_at())
            - Duration::milliseconds(time_to_subtract);

        let latest_allowable_expiry =
            Utc::now() + Duration::milliseconds(i64::from(self.max_expiry_duration));
        let new_expiry = if new_expiry > latest_allowable_expiry {
            println!("[MY_LOG TRACE] {} {}:{}",module_path!() ,file!(), line!()); trace!(
                "Shortening packet expiry duration to {}ms in the future",
                self.max_expiry_duration
            );
            latest_allowable_expiry
        } else {
            new_expiry
        };

        request.prepare.set_expires_at(new_expiry.into());
        self.next.send_request(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interledger_packet::{Address, ErrorCode, FulfillBuilder, PrepareBuilder, RejectBuilder};
    use interledger_service::{outgoing_service_fn, Username};
    use std::str::FromStr;
    use uuid::Uuid;

    use once_cell::sync::Lazy;

    pub static ALICE: Lazy<Username> = Lazy::new(|| Username::from_str("alice").unwrap());
    pub static EXAMPLE_ADDRESS: Lazy<Address> =
        Lazy::new(|| Address::from_str("example.alice").unwrap());

    #[derive(Clone, Debug)]
    struct TestAccount(Uuid, u32);
    impl Account for TestAccount {
        fn id(&self) -> Uuid {
            self.0
        }

        fn username(&self) -> &Username {
            &ALICE
        }

        fn asset_code(&self) -> &str {
            "XYZ"
        }

        // All connector accounts use asset scale = 9.
        fn asset_scale(&self) -> u8 {
            9
        }

        fn ilp_address(&self) -> &Address {
            &EXAMPLE_ADDRESS
        }
    }

    impl RoundTripTimeAccount for TestAccount {
        fn round_trip_time(&self) -> u32 {
            self.1
        }
    }

    #[tokio::test]
    async fn shortens_expiry_by_round_trip_time() {
        let original_expiry = Utc::now() + Duration::milliseconds(30000);
        let mut service = ExpiryShortenerService::new(outgoing_service_fn(move |request| {
            if DateTime::<Utc>::from(request.prepare.expires_at())
                == original_expiry - Duration::milliseconds(1300)
            {
                Ok(FulfillBuilder {
                    fulfillment: &[0; 32],
                    data: &[],
                }
                .build())
            } else {
                Err(RejectBuilder {
                    code: ErrorCode::F00_BAD_REQUEST,
                    message: &[],
                    data: &[],
                    triggered_by: None,
                }
                .build())
            }
        }));
        service
            .send_request(OutgoingRequest {
                from: TestAccount(Uuid::new_v4(), 600),
                to: TestAccount(Uuid::new_v4(), 700),
                prepare: PrepareBuilder {
                    destination: Address::from_str("example.destination").unwrap(),
                    amount: 10,
                    expires_at: original_expiry.into(),
                    data: &[],
                    execution_condition: &[0; 32],
                }
                .build(),
                original_amount: 10,
            })
            .await
            .expect("Should have shortened expiry");
    }

    #[tokio::test]
    async fn reduces_expiry_to_max_duration() {
        let mut service = ExpiryShortenerService::new(outgoing_service_fn(move |request| {
            if DateTime::<Utc>::from(request.prepare.expires_at()) - Utc::now()
                <= Duration::milliseconds(30000)
            {
                Ok(FulfillBuilder {
                    fulfillment: &[0; 32],
                    data: &[],
                }
                .build())
            } else {
                Err(RejectBuilder {
                    code: ErrorCode::F00_BAD_REQUEST,
                    message: &[],
                    data: &[],
                    triggered_by: None,
                }
                .build())
            }
        }));
        service
            .send_request(OutgoingRequest {
                from: TestAccount(Uuid::new_v4(), 500),
                to: TestAccount(Uuid::new_v4(), 500),
                prepare: PrepareBuilder {
                    destination: Address::from_str("example.destination").unwrap(),
                    amount: 10,
                    expires_at: (Utc::now() + Duration::milliseconds(45000)).into(),
                    data: &[],
                    execution_condition: &[0; 32],
                }
                .build(),
                original_amount: 10,
            })
            .await
            .expect("Should have shortened expiry");
    }
}
