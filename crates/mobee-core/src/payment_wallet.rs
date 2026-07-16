//! Wallet-backed payment policy, adapters, and authenticity checks.

use std::collections::HashSet;
use std::future::Future;
use std::str::FromStr;
use std::sync::mpsc;
use std::thread;

use cashu::{
    Amount, CurrencyUnit, MintUrl, PublicKey as CashuPublicKey, SecretKey, SpendingConditions,
    Token,
};
use cdk::wallet::{
    HttpClient, KeysetFilter, MintConnector, ReceiveOptions, SendOptions, Wallet,
};
use nostr_sdk::PublicKey as NostrPublicKey;

use crate::gateway::ParsedOffer;
use crate::payment::{
    AttemptId, EffectError, LockedPayment, PaymentEffects, PaymentKey, PaymentTerms,
    ReceiptEvidence,
};
use crate::payment_send::{PaymentPayload, PaymentSend, PaymentSent};
use crate::wallet::{TradeLock, VerifiedPayment, verify_trade_p2pk_with_connector};

const ATTEMPT_METADATA: &str = "mobee_attempt_id";

#[derive(Debug)]
/// Failure in a wallet-backed payment operation.
pub enum PaymentWalletError {
    Policy(String),
    Wallet(String),
    Reconcile(String),
    Verify(String),
    /// Predicted mint fee did not match the post-swap net credit.
    ///
    /// Wallet credit from the swap is left intact; callers must not journal or
    /// publish a receipt for this attempt.
    FeeMismatch {
        face: Amount,
        received: Amount,
        predicted_fee: Amount,
    },
}

impl std::fmt::Display for PaymentWalletError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Policy(message) => write!(formatter, "payment policy rejected: {message}"),
            Self::Wallet(message) => write!(formatter, "wallet operation failed: {message}"),
            Self::Reconcile(message) => {
                write!(formatter, "wallet reconciliation refused: {message}")
            }
            Self::Verify(message) => write!(formatter, "payment verification failed: {message}"),
            Self::FeeMismatch {
                face,
                received,
                predicted_fee,
            } => write!(
                formatter,
                "fee mismatch after swap: face={face} received={received} predicted_fee={predicted_fee} (wallet credit intact; do not journal)"
            ),
        }
    }
}

impl std::error::Error for PaymentWalletError {}

/// Constructs typed payment terms under an explicit mint allowlist.
pub struct PaymentPolicy {
    allowed_mints: HashSet<MintUrl>,
}

impl PaymentPolicy {
    /// Creates a policy from the complete allowed test-mint set.
    pub fn new(allowed_mints: impl IntoIterator<Item = MintUrl>) -> Self {
        Self {
            allowed_mints: allowed_mints.into_iter().collect(),
        }
    }

    /// Maps a validated offer and accepted seller into shared typed terms.
    pub fn terms_for_offer(
        &self,
        offer: &ParsedOffer,
        accepted_seller: &str,
    ) -> Result<PaymentTerms, PaymentWalletError> {
        offer
            .assert_seller_matches(accepted_seller)
            .map_err(|error| PaymentWalletError::Policy(error.to_string()))?;
        let mint = MintUrl::from_str(&offer.mint_url)
            .map_err(|error| PaymentWalletError::Policy(format!("invalid mint URL: {error}")))?;
        let unit = CurrencyUnit::from_str(&offer.unit).map_err(|error| {
            PaymentWalletError::Policy(format!(
                "unsupported payment unit {:?}: {error}",
                offer.unit
            ))
        })?;
        if unit != CurrencyUnit::Sat {
            return Err(PaymentWalletError::Policy(format!(
                "unsupported payment unit {:?}",
                offer.unit
            )));
        }
        if !self.allowed_mints.contains(&mint) {
            return Err(PaymentWalletError::Policy(format!(
                "mint {mint} is outside the test-mint allowlist"
            )));
        }
        let seller_nostr_pubkey = NostrPublicKey::parse(accepted_seller).map_err(|error| {
            PaymentWalletError::Policy(format!("invalid accepted seller key: {error}"))
        })?;
        let seller_p2pk_lock =
            CashuPublicKey::from_str(&format!("02{}", seller_nostr_pubkey.to_hex())).map_err(
                |error| PaymentWalletError::Policy(format!("invalid seller P2PK lock: {error}")),
            )?;

        Ok(PaymentTerms::new(
            mint,
            Amount::from(offer.amount),
            unit,
            seller_nostr_pubkey,
            seller_p2pk_lock,
        ))
    }
}

/// Buyer wallet adapter backed by CDK's persisted send sagas.
pub struct CdkBuyerMint<'a> {
    wallet: &'a Wallet,
}

impl<'a> CdkBuyerMint<'a> {
    /// Creates an adapter over one mint-and-unit wallet.
    pub fn new(wallet: &'a Wallet) -> Self {
        Self { wallet }
    }

    /// Returns the existing token for an attempt or creates one seller-locked send.
    pub async fn lock_or_reconcile(
        &self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
    ) -> Result<LockedPayment, PaymentWalletError> {
        require_wallet_matches(self.wallet, terms)?;
        self.recover_unmapped_sagas().await?;
        if let Some(token) = self.reconcile(attempt_id, terms).await? {
            return Ok(LockedPayment::new(token));
        }
        let mut options = SendOptions {
            conditions: Some(SpendingConditions::new_p2pk(terms.seller_p2pk_lock, None)),
            ..SendOptions::default()
        };
        options
            .metadata
            .insert(ATTEMPT_METADATA.into(), attempt_id.as_str().into());
        let prepared = self
            .wallet
            .prepare_send(terms.amount, options)
            .await
            .map_err(wallet_error)?;
        let token = prepared.confirm(None).await.map_err(wallet_error)?;
        Ok(LockedPayment::new(token))
    }

    async fn reconcile(
        &self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
    ) -> Result<Option<Token>, PaymentWalletError> {
        use cdk::wallet::types::TransactionDirection;

        let matches = self
            .wallet
            .list_transactions(Some(TransactionDirection::Outgoing))
            .await
            .map_err(wallet_error)?
            .into_iter()
            .filter(|transaction| {
                transaction
                    .metadata
                    .get(ATTEMPT_METADATA)
                    .map(String::as_str)
                    == Some(attempt_id.as_str())
            })
            .collect::<Vec<_>>();
        let transaction = match matches.as_slice() {
            [] => return Ok(None),
            [transaction] => transaction,
            _ => {
                return Err(PaymentWalletError::Reconcile(
                    "multiple wallet transactions claim the same payment attempt".into(),
                ));
            }
        };
        if transaction.mint_url != terms.mint
            || transaction.unit != terms.unit
            || transaction.amount != terms.amount
        {
            return Err(PaymentWalletError::Reconcile(
                "persisted wallet transaction does not match payment terms".into(),
            ));
        }
        let proofs = self
            .wallet
            .get_proofs_for_transaction(transaction.id())
            .await
            .map_err(wallet_error)?;
        let expected_ys = transaction.ys.iter().copied().collect::<HashSet<_>>();
        let actual_ys = proofs
            .iter()
            .map(|proof| proof.y())
            .collect::<Result<HashSet<_>, _>>()
            .map_err(wallet_error)?;
        if actual_ys != expected_ys {
            return Err(PaymentWalletError::Reconcile(
                "persisted payment proofs do not match the confirmed transaction".into(),
            ));
        }
        let token = Token::new(
            transaction.mint_url.clone(),
            proofs,
            transaction.memo.clone(),
            transaction.unit.clone(),
        );
        if token.value().map_err(wallet_error)? != terms.amount {
            return Err(PaymentWalletError::Reconcile(
                "persisted payment proofs do not match the confirmed amount".into(),
            ));
        }
        Ok(Some(token))
    }

    async fn recover_unmapped_sagas(&self) -> Result<(), PaymentWalletError> {
        if self
            .wallet
            .localstore
            .get_incomplete_sagas()
            .await
            .map_err(wallet_error)?
            .is_empty()
        {
            return Ok(());
        }
        Err(PaymentWalletError::Reconcile(
            "wallet has an incomplete operation with no matching confirmed attempt".into(),
        ))
    }
}

struct CdkPaymentVerifier<'a, C: ?Sized> {
    connector: &'a C,
}

impl<'a, C: ?Sized> CdkPaymentVerifier<'a, C> {
    fn new(connector: &'a C) -> Self {
        Self { connector }
    }
}

impl<C: MintConnector + ?Sized> CdkPaymentVerifier<'_, C> {
    async fn verify(
        &self,
        locked: &LockedPayment,
        terms: &PaymentTerms,
    ) -> Result<VerifiedPayment, PaymentWalletError> {
        let lock = TradeLock {
            mint: terms.mint.clone(),
            amount: terms.amount,
            unit: terms.unit.clone(),
            seller_lock: terms.seller_p2pk_lock,
        };
        verify_trade_p2pk_with_connector(self.connector, locked.token(), &lock)
            .await
            .map_err(|error| PaymentWalletError::Verify(error.to_string()))
    }
}

/// Seller wallet adapter whose successful receive is the mint-authenticity gate.
pub struct CdkSellerReceive<'a> {
    wallet: &'a Wallet,
    seller_key: SecretKey,
}

enum BuyerCommand {
    Lock {
        attempt_id: AttemptId,
        terms: PaymentTerms,
        response: mpsc::SyncSender<Result<LockedPayment, PaymentWalletError>>,
    },
    Verify {
        token: Token,
        terms: PaymentTerms,
        response: mpsc::SyncSender<Result<VerifiedPayment, PaymentWalletError>>,
    },
    Send {
        payload: PaymentPayload,
        response: mpsc::SyncSender<Result<PaymentSent, PaymentWalletError>>,
    },
}

/// Synchronous state-machine effects backed by one asynchronous wallet worker.
pub struct CdkPaymentEffects<R> {
    commands: Option<tokio::sync::mpsc::Sender<BuyerCommand>>,
    worker: Option<thread::JoinHandle<()>>,
    receipt: R,
}

impl<R> CdkPaymentEffects<R> {
    /// Starts a worker whose verifier is bound to the wallet mint.
    pub fn spawn<S>(wallet: Wallet, payment_send: S, receipt: R) -> Result<Self, PaymentWalletError>
    where
        S: PaymentSend + Send + 'static,
    {
        let connector = HttpClient::new(wallet.mint_url.clone(), None);
        Self::spawn_worker(wallet, connector, payment_send, receipt)
    }

    /// Starts a worker with an injected connector for hermetic tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_with_connector<C, S>(
        wallet: Wallet,
        connector: C,
        payment_send: S,
        receipt: R,
    ) -> Result<Self, PaymentWalletError>
    where
        C: MintConnector + Send + Sync + 'static,
        S: PaymentSend + Send + 'static,
    {
        Self::spawn_worker(wallet, connector, payment_send, receipt)
    }

    fn spawn_worker<C, S>(
        wallet: Wallet,
        connector: C,
        mut payment_send: S,
        receipt: R,
    ) -> Result<Self, PaymentWalletError>
    where
        C: MintConnector + Send + Sync + 'static,
        S: PaymentSend + Send + 'static,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(wallet_error)?;
        let (commands, mut requests) = tokio::sync::mpsc::channel(1);
        let worker = thread::Builder::new()
            .name("mobee-payment-wallet".into())
            .spawn(move || {
                runtime.block_on(async move {
                    while let Some(command) = requests.recv().await {
                        match command {
                            BuyerCommand::Lock {
                                attempt_id,
                                terms,
                                response,
                            } => {
                                let result = CdkBuyerMint::new(&wallet)
                                    .lock_or_reconcile(&attempt_id, &terms)
                                    .await;
                                let _ = response.send(result);
                            }
                            BuyerCommand::Verify {
                                token,
                                terms,
                                response,
                            } => {
                                let locked = LockedPayment::new(token);
                                let result = CdkPaymentVerifier::new(&connector)
                                    .verify(&locked, &terms)
                                    .await;
                                let _ = response.send(result);
                            }
                            BuyerCommand::Send { payload, response } => {
                                let result = payment_send
                                    .send_payment(payload)
                                    .await
                                    .map_err(|error| PaymentWalletError::Wallet(error.to_string()));
                                let _ = response.send(result);
                            }
                        }
                    }
                });
            })
            .map_err(wallet_error)?;
        Ok(Self {
            commands: Some(commands),
            worker: Some(worker),
            receipt,
        })
    }

    fn request<T>(
        &self,
        command: impl FnOnce(mpsc::SyncSender<Result<T, PaymentWalletError>>) -> BuyerCommand,
    ) -> Result<T, EffectError> {
        let (response, result) = mpsc::sync_channel(1);
        self.commands
            .as_ref()
            .ok_or_else(|| EffectError::new("payment wallet worker is stopped"))?
            .try_send(command(response))
            .map_err(|error| {
                EffectError::new(format!("payment wallet worker unavailable: {error}"))
            })?;
        result
            .recv()
            .map_err(|_| EffectError::new("payment wallet worker dropped its response"))?
            .map_err(|error| EffectError::new(error.to_string()))
    }
}

impl<R> Drop for CdkPaymentEffects<R> {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl<R> PaymentEffects for CdkPaymentEffects<R>
where
    R: FnMut(&PaymentKey, &PaymentSent) -> Result<ReceiptEvidence, EffectError>,
{
    fn lock_or_reconcile(
        &mut self,
        attempt_id: &AttemptId,
        terms: &PaymentTerms,
    ) -> Result<LockedPayment, EffectError> {
        self.request(|response| BuyerCommand::Lock {
            attempt_id: attempt_id.clone(),
            terms: terms.clone(),
            response,
        })
    }

    fn verify_payment(
        &mut self,
        _attempt_id: &AttemptId,
        terms: &PaymentTerms,
        locked: &LockedPayment,
    ) -> Result<VerifiedPayment, EffectError> {
        self.request(|response| BuyerCommand::Verify {
            token: locked.token().clone(),
            terms: terms.clone(),
            response,
        })
    }

    fn send_payment(
        &mut self,
        key: &PaymentKey,
        _attempt_id: &AttemptId,
        terms: &PaymentTerms,
        locked: &LockedPayment,
        _verified: &VerifiedPayment,
    ) -> Result<PaymentSent, EffectError> {
        if terms.unit != CurrencyUnit::Sat {
            return Err(EffectError::new(
                "NIP-17 v0 payment payload supports sat only",
            ));
        }
        let payload = PaymentPayload {
            job_id: key.job_id.as_str().into(),
            result_id: key.result_id.as_str().into(),
            mint_url: terms.mint.to_string(),
            amount: terms.amount.to_u64(),
            unit: terms.unit.to_string(),
            token: locked.token().clone(),
            seller_pubkey: terms.seller_nostr_pubkey.to_hex(),
        };
        self.request(|response| BuyerCommand::Send { payload, response })
    }

    fn publish_receipt(
        &mut self,
        key: &PaymentKey,
        payment: &PaymentSent,
    ) -> Result<ReceiptEvidence, EffectError> {
        (self.receipt)(key, payment)
    }
}

impl<'a> CdkSellerReceive<'a> {
    /// Creates a receive adapter for the seller's mint wallet and P2PK key.
    pub fn new(wallet: &'a Wallet, seller_key: SecretKey) -> Self {
        Self { wallet, seller_key }
    }

    /// Swaps the received token at its mint before returning its redeemable amount.
    pub async fn receive(
        &self,
        token: &Token,
        terms: &PaymentTerms,
    ) -> Result<Amount, PaymentWalletError> {
        self.receive_with(token, terms, |options| async move {
            self.wallet
                .receive(&token.to_string(), options)
                .await
                .map_err(wallet_error)
        })
        .await
    }

    async fn receive_with<F, Fut>(
        &self,
        token: &Token,
        terms: &PaymentTerms,
        receive: F,
    ) -> Result<Amount, PaymentWalletError>
    where
        F: FnOnce(ReceiveOptions) -> Fut,
        Fut: Future<Output = Result<Amount, PaymentWalletError>>,
    {
        require_wallet_matches(self.wallet, terms)?;
        let token_mint = token.mint_url().map_err(wallet_error)?;
        let face = token.value().map_err(wallet_error)?;
        if token_mint != terms.mint
            || token.unit().as_ref() != Some(&terms.unit)
            || face != terms.amount
        {
            return Err(PaymentWalletError::Policy(
                "received token does not match payment terms".into(),
            ));
        }
        if self.seller_key.public_key().x_only_public_key()
            != terms.seller_p2pk_lock.x_only_public_key()
        {
            return Err(PaymentWalletError::Policy(
                "seller receive key does not match payment terms".into(),
            ));
        }

        // Fee must be predicted pre-swap: CDK receive returns net after fees.
        let keysets = self
            .wallet
            .get_mint_keysets(KeysetFilter::All)
            .await
            .map_err(wallet_error)?;
        let proofs = token.proofs(&keysets).map_err(wallet_error)?;
        let fee = self
            .wallet
            .get_proofs_fee(&proofs)
            .await
            .map_err(wallet_error)?
            .total;
        if face <= fee {
            return Err(PaymentWalletError::Policy(format!(
                "token uneconomical vs mint fee: face={face} fee={fee}"
            )));
        }

        let options = ReceiveOptions {
            p2pk_signing_keys: vec![self.seller_key.clone()],
            ..ReceiveOptions::default()
        };
        let received = receive(options).await?;
        require_received_amount_after_fee(received, face, fee)
    }
}

fn require_received_amount_after_fee(
    received: Amount,
    face: Amount,
    fee: Amount,
) -> Result<Amount, PaymentWalletError> {
    // Journal/daemon invariants expect face (== offer.amount), not wallet net.
    if received
        .checked_add(fee)
        .is_some_and(|total| total == face)
    {
        return Ok(face);
    }
    if received > Amount::ZERO {
        return Err(PaymentWalletError::FeeMismatch {
            face,
            received,
            predicted_fee: fee,
        });
    }
    Err(PaymentWalletError::Policy(
        "received amount does not match payment terms".into(),
    ))
}

fn require_wallet_matches(wallet: &Wallet, terms: &PaymentTerms) -> Result<(), PaymentWalletError> {
    if wallet.mint_url != terms.mint || wallet.unit != terms.unit {
        return Err(PaymentWalletError::Policy(
            "wallet mint or unit does not match payment terms".into(),
        ));
    }
    Ok(())
}

fn wallet_error(error: impl std::fmt::Display) -> PaymentWalletError {
    PaymentWalletError::Wallet(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use cashu::secret::Secret;
    use cashu::{
        Conditions, Id, KeySet, KeySetInfo, Keys, MintInfo, Proof, ProofState, PublicKey, State,
    };
    use cdk::cdk_database::WalletDatabase;
    use cdk::wallet::types::{ProofInfo, Transaction, TransactionDirection, WalletSaga};
    use cdk::wallet::{BaseHttpClient, HttpTransport, Wallet, WalletBuilder};
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use url::Url;

    use super::*;
    use crate::gateway::ParsedOffer;
    use crate::delivery::{
        CommitOid, DeliveryError, DeliveryVerifier, GitDelivery, VerifiedDelivery,
    };
    use crate::payment::{
        DeliveryIntegrityHash, JobHash, JobId, MemoryPaymentJournal, PaymentKey, PaymentService,
        PaymentState, ReceiptAuthority, ResultId,
    };
    use crate::payment_send::{PaymentSendError, PaymentSent};

    /// Test-only accept verifier so wallet spine tests go through `run_with_verifier`
    /// (delivery tip-bind) instead of the now module-private `advance`.
    struct AcceptDelivery;

    impl DeliveryVerifier for AcceptDelivery {
        fn verify(
            &mut self,
            delivery: &GitDelivery,
        ) -> Result<VerifiedDelivery, DeliveryError> {
            VerifiedDelivery::from_fetched_tip(delivery, delivery.commit_oid().clone())
        }
    }

    const MINT: &str = "https://testnut.cashu.space";
    const OTHER_MINT: &str = "https://real-mint.example";
    const KEYSET_ID: &str = "009a1f293253e41e";

    #[test]
    fn policy_rejects_a_mint_outside_the_allowlist() {
        let seller = secret_key(1).public_key().to_string();
        let policy = PaymentPolicy::new([mint(MINT)]);
        let offer = offer(OTHER_MINT, &seller);

        let error = policy.terms_for_offer(&offer, &seller).unwrap_err();

        assert!(matches!(
            error,
            PaymentWalletError::Policy(message) if message.contains("outside the test-mint allowlist")
        ));
    }

    #[test]
    fn policy_maps_the_offer_once_into_typed_terms() {
        let seller_lock = secret_key(1).public_key();
        let seller = nostr_key_for_p2pk(seller_lock).to_hex();
        let policy = PaymentPolicy::new([mint(MINT)]);

        let terms = policy
            .terms_for_offer(&offer(MINT, &seller), &seller)
            .unwrap();

        assert_eq!(terms.mint, mint(MINT));
        assert_eq!(terms.amount, Amount::from(7));
        assert_eq!(terms.unit, CurrencyUnit::Sat);
        assert_eq!(terms.seller_nostr_pubkey.to_hex(), seller);
        assert_eq!(
            terms.seller_p2pk_lock.x_only_public_key(),
            seller_lock.x_only_public_key()
        );
    }

    #[test]
    fn policy_rejects_an_unknown_unit_without_defaulting_to_sat() {
        let seller = secret_key(1).public_key().to_string();
        let policy = PaymentPolicy::new([mint(MINT)]);
        let mut offer = offer(MINT, &seller);
        offer.unit = "credit".into();

        let result = policy.terms_for_offer(&offer, &seller);

        assert!(matches!(
            result,
            Err(PaymentWalletError::Policy(message))
                if message.contains("unsupported payment unit")
        ));
    }

    #[tokio::test]
    async fn confirmed_attempt_reconciles_the_exact_token_without_a_second_send() {
        let fixture = wallet_fixture().await;
        let key = payment_key(&fixture.terms);
        let attempt_id = key.attempt_id();
        store_confirmed_attempt(&fixture.wallet, &attempt_id, &fixture.token).await;

        let locked = CdkBuyerMint::new(&fixture.wallet)
            .lock_or_reconcile(&attempt_id, &fixture.terms)
            .await
            .unwrap();

        assert_eq!(locked.token(), &fixture.token);
        assert_eq!(
            fixture
                .wallet
                .list_transactions(Some(TransactionDirection::Outgoing))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn confirmed_attempt_without_its_exact_proof_refuses() {
        let fixture = wallet_fixture().await;
        let key = payment_key(&fixture.terms);
        let attempt_id = key.attempt_id();
        store_confirmed_transaction(&fixture.wallet, &attempt_id, &fixture.proof).await;

        let result = CdkBuyerMint::new(&fixture.wallet)
            .lock_or_reconcile(&attempt_id, &fixture.terms)
            .await;

        assert!(matches!(
            result,
            Err(PaymentWalletError::Reconcile(message))
                if message.contains("proofs do not match the confirmed transaction")
        ));
    }

    #[tokio::test]
    async fn unbound_incomplete_saga_refuses_instead_of_guessing_or_reminting() {
        let fixture = wallet_fixture().await;
        let key = payment_key(&fixture.terms);
        let saga = WalletSaga::new(
            uuid::Uuid::now_v7(),
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::ProofsReserved,
            ),
            fixture.terms.amount,
            fixture.terms.mint.clone(),
            fixture.terms.unit.clone(),
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: fixture.terms.amount,
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: None,
            }),
        );
        fixture.wallet.localstore.add_saga(saga).await.unwrap();

        let result = CdkBuyerMint::new(&fixture.wallet)
            .lock_or_reconcile(&key.attempt_id(), &fixture.terms)
            .await;

        assert!(matches!(
            result,
            Err(PaymentWalletError::Reconcile(message))
                if message.contains("incomplete operation with no matching confirmed attempt")
        ));
        assert!(
            fixture
                .wallet
                .list_transactions(Some(TransactionDirection::Outgoing))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn confirmed_attempt_with_an_unrelated_incomplete_saga_refuses() {
        let fixture = wallet_fixture().await;
        let key = payment_key(&fixture.terms);
        let attempt_id = key.attempt_id();
        store_confirmed_attempt(&fixture.wallet, &attempt_id, &fixture.token).await;
        let saga = WalletSaga::new(
            uuid::Uuid::now_v7(),
            cdk::wallet::types::WalletSagaState::Send(
                cdk::wallet::types::SendSagaState::ProofsReserved,
            ),
            fixture.terms.amount,
            fixture.terms.mint.clone(),
            fixture.terms.unit.clone(),
            cdk::wallet::types::OperationData::Send(cdk::wallet::types::SendOperationData {
                amount: fixture.terms.amount,
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: None,
            }),
        );
        fixture.wallet.localstore.add_saga(saga).await.unwrap();

        let result = CdkBuyerMint::new(&fixture.wallet)
            .lock_or_reconcile(&attempt_id, &fixture.terms)
            .await;

        assert!(matches!(result, Err(PaymentWalletError::Reconcile(_))));
    }

    #[tokio::test]
    async fn seller_receive_rejects_an_inflated_proof_at_the_mint_swap() {
        let seller_key = secret_key(1);
        let keyset = test_keyset();
        let proof = p2pk_proof_for_keyset(7, seller_key.public_key(), keyset.id);
        let proof_y = proof.y().unwrap();
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let transport = InflatedSwapTransport::new(proof_y, Amount::from(1));
        let swap_calls = transport.swap_calls.clone();
        let presented_amount = transport.presented_amount.clone();
        let wallet = seller_wallet(transport, keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );

        let result = CdkSellerReceive::new(&wallet, seller_key)
            .receive(&token, &terms)
            .await;

        assert!(matches!(result, Err(PaymentWalletError::Wallet(_))));
        assert_eq!(swap_calls.load(Ordering::SeqCst), 1);
        assert_eq!(presented_amount.load(Ordering::SeqCst), 7);
    }

    #[tokio::test]
    async fn seller_receive_rejects_an_authentic_underpay_before_the_mint_swap() {
        let seller_key = secret_key(1);
        let keyset = test_keyset();
        let proof = p2pk_proof_for_keyset(1, seller_key.public_key(), keyset.id);
        let proof_y = proof.y().unwrap();
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let transport = InflatedSwapTransport::new(proof_y, Amount::from(7));
        let swap_calls = transport.swap_calls.clone();
        let wallet = seller_wallet(transport, keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );

        let result = CdkSellerReceive::new(&wallet, seller_key)
            .receive(&token, &terms)
            .await;

        assert!(matches!(result, Err(PaymentWalletError::Policy(_))));
        assert_eq!(swap_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn seller_receive_rejects_dust_as_uneconomical_before_swap() {
        let seller_key = secret_key(1);
        let keyset = test_keyset_with_fee(1_000); // 1 proof → fee = 1
        let proof = p2pk_proof_for_keyset(1, seller_key.public_key(), keyset.id);
        let proof_y = proof.y().unwrap();
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let transport = InflatedSwapTransport::new(proof_y, Amount::from(1));
        let swap_calls = transport.swap_calls.clone();
        let wallet = seller_wallet(transport, keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(1),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );

        let result = CdkSellerReceive::new(&wallet, seller_key)
            .receive(&token, &terms)
            .await;

        assert!(matches!(
            result,
            Err(PaymentWalletError::Policy(message))
                if message.contains("uneconomical vs mint fee")
        ));
        assert_eq!(swap_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn seller_receive_returns_face_when_net_plus_fee_matches() {
        let seller_key = secret_key(1);
        let keyset = test_keyset_with_fee(1_000); // 1 proof → fee = 1
        let proof = p2pk_proof_for_keyset(2, seller_key.public_key(), keyset.id);
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let wallet = seller_wallet(InflatedSwapTransport::default(), keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(2),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );
        let adapter = CdkSellerReceive::new(&wallet, seller_key);

        // CDK receive returns net after fees (face 2 − fee 1 = 1).
        let amount = adapter
            .receive_with(&token, &terms, |_| async { Ok(Amount::from(1)) })
            .await
            .unwrap();

        assert_eq!(amount, Amount::from(2));
    }

    #[tokio::test]
    async fn seller_receive_surfaces_fee_mismatch_without_treating_as_underpay() {
        let seller_key = secret_key(1);
        let keyset = test_keyset_with_fee(1_000); // 1 proof → fee = 1
        let proof = p2pk_proof_for_keyset(2, seller_key.public_key(), keyset.id);
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let wallet = seller_wallet(InflatedSwapTransport::default(), keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(2),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );
        let adapter = CdkSellerReceive::new(&wallet, seller_key);

        let result = adapter
            .receive_with(&token, &terms, |_| async { Ok(Amount::from(2)) })
            .await;

        assert!(matches!(
            result,
            Err(PaymentWalletError::FeeMismatch {
                face,
                received,
                predicted_fee,
            }) if face == Amount::from(2)
                && received == Amount::from(2)
                && predicted_fee == Amount::from(1)
        ));
    }

    #[tokio::test]
    async fn seller_receive_rejects_when_wallet_returns_a_mismatched_amount() {
        let seller_key = secret_key(1);
        let keyset = test_keyset(); // fee = 0
        let proof = p2pk_proof_for_keyset(7, seller_key.public_key(), keyset.id);
        let token = Token::new(mint(MINT), vec![proof], None, CurrencyUnit::Sat);
        let wallet = seller_wallet(InflatedSwapTransport::default(), keyset).await;
        let terms = PaymentTerms::new(
            mint(MINT),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller_key.public_key()),
            seller_key.public_key(),
        );
        let adapter = CdkSellerReceive::new(&wallet, seller_key);

        let result = adapter
            .receive_with(&token, &terms, |_| async {
                Ok(Amount::from(1))
            })
            .await;

        assert!(matches!(
            result,
            Err(PaymentWalletError::FeeMismatch {
                face,
                received,
                predicted_fee,
            }) if face == Amount::from(7)
                && received == Amount::from(1)
                && predicted_fee == Amount::ZERO
        ));
    }

    #[test]
    fn worker_wires_reconcile_verify_and_send_into_the_real_state_machine() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let fixture = runtime.block_on(wallet_fixture());
        let key = payment_key(&fixture.terms);
        runtime.block_on(store_confirmed_attempt(
            &fixture.wallet,
            &key.attempt_id(),
            &fixture.token,
        ));
        let ys = [fixture.proof.y().unwrap()];
        let connector = BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse {
                states: ys
                    .iter()
                    .copied()
                    .map(|y| ProofState::from((y, State::Unspent)))
                    .collect(),
            }),
            None,
        );
        let send_count = Arc::new(AtomicUsize::new(0));
        let sender = CountingSend(send_count.clone());
        let authority = authority();
        let receipt_authority = authority.clone();
        let mut effects = CdkPaymentEffects::spawn_with_connector(
            fixture.wallet.clone(),
            connector,
            sender,
            move |_: &PaymentKey, _: &PaymentSent| {
                Ok(ReceiptEvidence {
                    receipt_id: "receipt".into(),
                    author: receipt_authority.buyer,
                    valid_signers: vec![receipt_authority.buyer, receipt_authority.seller],
                })
            },
        )
        .unwrap();
        let journal = MemoryPaymentJournal::default();
        let delivery = git_delivery_for_key(&key);
        let mut verifier = AcceptDelivery;

        let state = PaymentService::new(&journal)
            .run_with_verifier(
                &delivery,
                &mut verifier,
                &key,
                &fixture.terms,
                &authority,
                &mut effects,
            )
            .unwrap();

        assert!(matches!(state, PaymentState::Closed { .. }));
        assert_eq!(send_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn worker_sends_to_the_nostr_identity_not_the_odd_parity_p2pk_lock() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let seller_key = odd_secret_key();
        let fixture = runtime.block_on(wallet_fixture_for_seller(seller_key.public_key()));
        let key = payment_key(&fixture.terms);
        runtime.block_on(store_confirmed_attempt(
            &fixture.wallet,
            &key.attempt_id(),
            &fixture.token,
        ));
        let proof_y = fixture.proof.y().unwrap();
        let connector = BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse {
                states: vec![ProofState::from((proof_y, State::Unspent))],
            }),
            None,
        );
        let authority = authority();
        let receipt_authority = authority.clone();
        let mut effects = CdkPaymentEffects::spawn_with_connector(
            fixture.wallet.clone(),
            connector,
            NostrRecipientSend,
            move |_: &PaymentKey, _: &PaymentSent| {
                Ok(ReceiptEvidence {
                    receipt_id: "receipt".into(),
                    author: receipt_authority.buyer,
                    valid_signers: vec![receipt_authority.buyer, receipt_authority.seller],
                })
            },
        )
        .unwrap();
        let journal = MemoryPaymentJournal::default();
        let delivery = git_delivery_for_key(&key);
        let mut verifier = AcceptDelivery;

        let state = PaymentService::new(&journal)
            .run_with_verifier(
                &delivery,
                &mut verifier,
                &key,
                &fixture.terms,
                &authority,
                &mut effects,
            )
            .unwrap();

        assert!(matches!(state, PaymentState::Closed { .. }));
    }

    #[test]
    fn worker_rejects_a_wrong_seller_lock_through_the_real_verify_adapter() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let fixture = runtime.block_on(wallet_fixture());
        let proof_y = fixture.proof.y().unwrap();
        let connector = BaseHttpClient::with_transport(
            mint(MINT),
            CheckStateTransport::new(cashu::CheckStateResponse {
                states: vec![ProofState::from((proof_y, State::Unspent))],
            }),
            None,
        );
        let mut effects = CdkPaymentEffects::spawn_with_connector(
            fixture.wallet.clone(),
            connector,
            CountingSend(Arc::new(AtomicUsize::new(0))),
            |_: &PaymentKey, _: &PaymentSent| unreachable!(),
        )
        .unwrap();
        let wrong_terms = wallet_terms(secret_key(2).public_key());
        let locked = LockedPayment::new(fixture.token.clone());

        let result = effects.verify_payment(
            &payment_key(&wrong_terms).attempt_id(),
            &wrong_terms,
            &locked,
        );

        assert!(result.is_err());
    }

    struct WalletFixture {
        wallet: Wallet,
        terms: PaymentTerms,
        token: Token,
        proof: Proof,
    }

    async fn wallet_fixture() -> WalletFixture {
        wallet_fixture_for_seller(secret_key(1).public_key()).await
    }

    async fn wallet_fixture_for_seller(seller: PublicKey) -> WalletFixture {
        let proof = p2pk_proof(7, seller);
        let token = Token::new(mint(MINT), vec![proof.clone()], None, CurrencyUnit::Sat);
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        let wallet = Wallet::new(MINT, CurrencyUnit::Sat, store, [7; 64], None).unwrap();
        WalletFixture {
            wallet,
            terms: wallet_terms(seller),
            token,
            proof,
        }
    }

    async fn seller_wallet(transport: InflatedSwapTransport, keyset: KeySet) -> Wallet {
        let store = Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap());
        store
            .add_mint(mint(MINT), Some(MintInfo::new()))
            .await
            .unwrap();
        store
            .add_mint_keysets(
                mint(MINT),
                vec![KeySetInfo {
                    id: keyset.id,
                    unit: keyset.unit.clone(),
                    active: true,
                    input_fee_ppk: keyset.input_fee_ppk,
                    final_expiry: keyset.final_expiry,
                }],
            )
            .await
            .unwrap();
        store.add_keys(keyset).await.unwrap();
        let connector = Arc::new(BaseHttpClient::with_transport(mint(MINT), transport, None));
        WalletBuilder::new()
            .mint_url(mint(MINT))
            .unit(CurrencyUnit::Sat)
            .localstore(store)
            .seed([8; 64])
            .shared_client(connector)
            .build()
            .unwrap()
    }

    fn test_keyset() -> KeySet {
        test_keyset_with_fee(0)
    }

    fn test_keyset_with_fee(input_fee_ppk: u64) -> KeySet {
        let keys = [1_u64, 2, 4, 8]
            .into_iter()
            .map(|amount| {
                (
                    Amount::from(amount),
                    secret_key(amount as u8 + 10).public_key(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let keys = Keys::new(keys);
        KeySet {
            id: Id::v1_from_keys(&keys),
            unit: CurrencyUnit::Sat,
            active: Some(true),
            keys,
            input_fee_ppk,
            final_expiry: None,
        }
    }

    async fn store_confirmed_attempt(wallet: &Wallet, attempt_id: &AttemptId, token: &Token) {
        let proof = match token {
            Token::TokenV4(token) => token.token[0].proofs[0]
                .clone()
                .into_proof(&Id::from_str(KEYSET_ID).unwrap()),
            Token::TokenV3(_) => panic!("fixture uses v4 token"),
        };
        let proof_info = ProofInfo::new(
            proof.clone(),
            wallet.mint_url.clone(),
            State::PendingSpent,
            wallet.unit.clone(),
        )
        .unwrap();
        wallet
            .localstore
            .update_proofs(vec![proof_info], vec![])
            .await
            .unwrap();
        store_confirmed_transaction(wallet, attempt_id, &proof).await;
    }

    async fn store_confirmed_transaction(wallet: &Wallet, attempt_id: &AttemptId, proof: &Proof) {
        let mut metadata = HashMap::new();
        metadata.insert(ATTEMPT_METADATA.into(), attempt_id.as_str().into());
        wallet
            .localstore
            .add_transaction(Transaction {
                mint_url: wallet.mint_url.clone(),
                direction: TransactionDirection::Outgoing,
                amount: Amount::from(7),
                fee: Amount::ZERO,
                unit: wallet.unit.clone(),
                ys: vec![proof.y().unwrap()],
                timestamp: 1,
                memo: None,
                metadata,
                quote_id: None,
                payment_request: None,
                payment_proof: None,
                payment_method: None,
                saga_id: Some(uuid::Uuid::now_v7()),
            })
            .await
            .unwrap();
    }

    fn payment_key(terms: &PaymentTerms) -> PaymentKey {
        PaymentKey::new(
            JobId::new("job").unwrap(),
            ResultId::new("result").unwrap(),
            DeliveryIntegrityHash::from_hex("11".repeat(32)).unwrap(),
            JobHash::from_hex("22".repeat(32)).unwrap(),
            terms,
        )
    }

    fn git_delivery_for_key(key: &PaymentKey) -> GitDelivery {
        GitDelivery::new(
            "https://example.invalid/repo.git",
            "mobee/job",
            CommitOid::parse(key.delivery_integrity_hash.as_str()).unwrap(),
        )
        .unwrap()
    }

    fn offer(mint_url: &str, seller: &str) -> ParsedOffer {
        ParsedOffer {
            task: "task".into(),
            output: "text/plain".into(),
            amount: 7,
            unit: "sat".into(),
            deadline_unix: 1,
            mint_url: mint_url.into(),
            seller_pubkey: Some(seller.into()),
        }
    }

    fn authority() -> ReceiptAuthority {
        ReceiptAuthority {
            buyer: secret_key(2).public_key(),
            seller: secret_key(1).public_key(),
        }
    }

    fn p2pk_proof(amount: u64, seller: PublicKey) -> Proof {
        p2pk_proof_for_keyset(amount, seller, Id::from_str(KEYSET_ID).unwrap())
    }

    fn p2pk_proof_for_keyset(amount: u64, seller: PublicKey, keyset_id: Id) -> Proof {
        let secret = Secret::try_from(SpendingConditions::new_p2pk(
            seller,
            Some(Conditions::default()),
        ))
        .unwrap();
        Proof::new(
            Amount::from(amount),
            keyset_id,
            secret,
            secret_key(9).public_key(),
        )
    }

    fn secret_key(byte: u8) -> SecretKey {
        SecretKey::from_slice(&[byte; 32]).unwrap()
    }

    fn odd_secret_key() -> SecretKey {
        (1..=u8::MAX)
            .map(secret_key)
            .find(|key| key.public_key().to_string().starts_with("03"))
            .expect("an odd-parity test key exists")
    }

    fn nostr_key_for_p2pk(key: PublicKey) -> NostrPublicKey {
        let compressed = key.to_string();
        NostrPublicKey::from_hex(&compressed[2..]).unwrap()
    }

    fn wallet_terms(seller: PublicKey) -> PaymentTerms {
        PaymentTerms::new(
            mint(MINT),
            Amount::from(7),
            CurrencyUnit::Sat,
            nostr_key_for_p2pk(seller),
            seller,
        )
    }

    fn mint(url: &str) -> MintUrl {
        MintUrl::from_str(url).unwrap()
    }

    struct CountingSend(Arc<AtomicUsize>);

    struct NostrRecipientSend;

    impl PaymentSend for NostrRecipientSend {
        async fn send_payment(
            &mut self,
            payload: PaymentPayload,
        ) -> Result<PaymentSent, PaymentSendError> {
            nostr_sdk::PublicKey::parse(&payload.seller_pubkey).map_err(|error| {
                PaymentSendError::Transport(format!("invalid Nostr recipient: {error}"))
            })?;
            Ok(PaymentSent {
                payment_id: "payment".into(),
                relay_success: vec!["memory://relay".into()],
                relay_failed: Vec::new(),
            })
        }
    }

    impl PaymentSend for CountingSend {
        async fn send_payment(
            &mut self,
            _payload: PaymentPayload,
        ) -> Result<PaymentSent, PaymentSendError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(PaymentSent {
                payment_id: "payment".into(),
                relay_success: vec!["memory://relay".into()],
                relay_failed: Vec::new(),
            })
        }
    }

    #[derive(Clone, Debug, Default)]
    struct CheckStateTransport {
        response: serde_json::Value,
    }

    impl CheckStateTransport {
        fn new(response: cashu::CheckStateResponse) -> Self {
            Self {
                response: serde_json::to_value(response).unwrap(),
            }
        }
    }

    #[async_trait::async_trait]
    impl HttpTransport for CheckStateTransport {
        fn with_proxy(
            &mut self,
            _proxy: Url,
            _host_matcher: Option<&str>,
            _accept_invalid_certs: bool,
        ) -> Result<(), cdk::Error> {
            Ok(())
        }

        async fn http_get<R>(
            &self,
            _url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
        ) -> Result<R, cdk::Error>
        where
            R: DeserializeOwned,
        {
            Err(cdk::Error::Custom("unexpected GET".into()))
        }

        async fn http_post<P, R>(
            &self,
            url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
            _payload: &P,
        ) -> Result<R, cdk::Error>
        where
            P: Serialize + ?Sized + Send + Sync,
            R: DeserializeOwned,
        {
            if !url.path().ends_with("/v1/checkstate") {
                return Err(cdk::Error::Custom("unexpected POST".into()));
            }
            serde_json::from_value(self.response.clone())
                .map_err(|error| cdk::Error::Custom(error.to_string()))
        }
    }

    #[derive(Clone, Debug)]
    struct InflatedSwapTransport {
        authoritative_y: PublicKey,
        authoritative_amount: Amount,
        swap_calls: Arc<AtomicUsize>,
        presented_amount: Arc<AtomicU64>,
    }

    impl InflatedSwapTransport {
        fn new(authoritative_y: PublicKey, authoritative_amount: Amount) -> Self {
            Self {
                authoritative_y,
                authoritative_amount,
                swap_calls: Arc::new(AtomicUsize::new(0)),
                presented_amount: Arc::new(AtomicU64::new(0)),
            }
        }
    }

    impl Default for InflatedSwapTransport {
        fn default() -> Self {
            Self::new(secret_key(31).public_key(), Amount::ZERO)
        }
    }

    #[async_trait::async_trait]
    impl HttpTransport for InflatedSwapTransport {
        fn with_proxy(
            &mut self,
            _proxy: Url,
            _host_matcher: Option<&str>,
            _accept_invalid_certs: bool,
        ) -> Result<(), cdk::Error> {
            Ok(())
        }

        async fn http_get<R>(
            &self,
            _url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
        ) -> Result<R, cdk::Error>
        where
            R: DeserializeOwned,
        {
            Err(cdk::Error::Custom("unexpected GET".into()))
        }

        async fn http_post<P, R>(
            &self,
            url: Url,
            _auth: Option<cashu::nuts::AuthToken>,
            payload: &P,
        ) -> Result<R, cdk::Error>
        where
            P: Serialize + ?Sized + Send + Sync,
            R: DeserializeOwned,
        {
            if !url.path().ends_with("/v1/swap") {
                return Err(cdk::Error::Custom("unexpected POST".into()));
            }
            let request: cashu::SwapRequest = serde_json::from_value(
                serde_json::to_value(payload)
                    .map_err(|error| cdk::Error::Custom(error.to_string()))?,
            )
            .map_err(|error| cdk::Error::Custom(error.to_string()))?;
            let presented = request
                .input_amount()
                .map_err(|error| cdk::Error::Custom(error.to_string()))?;
            let presented_y = request
                .inputs()
                .first()
                .ok_or_else(|| cdk::Error::Custom("swap has no input".into()))?
                .y()
                .map_err(|error| cdk::Error::Custom(error.to_string()))?;
            if presented_y != self.authoritative_y || presented == self.authoritative_amount {
                return Err(cdk::Error::Custom(
                    "swap did not present the expected inflated unspent proof".into(),
                ));
            }
            self.swap_calls.fetch_add(1, Ordering::SeqCst);
            self.presented_amount
                .store(presented.to_u64(), Ordering::SeqCst);
            Err(cdk::Error::TransactionUnbalanced(
                self.authoritative_amount.to_u64(),
                presented.to_u64(),
                0,
            ))
        }
    }
}
