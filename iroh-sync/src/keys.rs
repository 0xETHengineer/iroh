//! Keys used in iroh-sync

use std::{cmp::Ordering, fmt, str::FromStr};

use ed25519_dalek::{Signature, SignatureError, Signer, SigningKey, VerifyingKey};
use rand_core::CryptoRngCore;
use serde::{Deserialize, Serialize};

/// Author key to insert entries in a [`Replica`]
///
/// Internally, an author is a [`SigningKey`] which is used to sign entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    priv_key: SigningKey,
}
impl Author {
    /// Create a new author with a random key.
    pub fn new<R: CryptoRngCore + ?Sized>(rng: &mut R) -> Self {
        let priv_key = SigningKey::generate(rng);
        Author { priv_key }
    }

    /// Create an author from a byte array.
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        SigningKey::from_bytes(bytes).into()
    }

    /// Returns the Author byte representation.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.priv_key.to_bytes()
    }

    /// Returns the AuthorId byte representation.
    pub fn id_bytes(&self) -> [u8; 32] {
        self.priv_key.verifying_key().to_bytes()
    }

    /// Get the [`AuthorId`] for this author.
    pub fn id(&self) -> AuthorId {
        AuthorId(self.priv_key.verifying_key())
    }

    /// Sign a message with this author key.
    pub fn sign(&self, msg: &[u8]) -> Signature {
        self.priv_key.sign(msg)
    }

    /// Strictly verify a signature on a message with this author's public key.
    pub fn verify(&self, msg: &[u8], signature: &Signature) -> Result<(), SignatureError> {
        self.priv_key.verify_strict(msg, signature)
    }
}

/// Identifier for an [`Author`]
///
/// This is the corresponding [`VerifyingKey`] for an author. It is used as an identifier, and can
/// be used to verify [`Signature`]s.
#[derive(Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct AuthorId(VerifyingKey);

impl AuthorId {
    /// Verify that a signature matches the `msg` bytes and was created with this the [`Author`]
    /// that corresponds to this [`AuthorId`].
    pub fn verify(&self, msg: &[u8], signature: &Signature) -> Result<(), SignatureError> {
        self.0.verify_strict(msg, signature)
    }

    /// Get the byte representation of this [`AuthorId`].
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Construct an `AuthorId` from a slice of bytes.
    ///
    /// # Warning
    ///
    /// The caller is responsible for ensuring that the bytes passed into this method actually
    /// represent a valid [`ed25591`] curve point. This will never fail for bytes returned from
    /// [`Self::as_bytes`].
    pub fn from_bytes(bytes: &[u8; 32]) -> anyhow::Result<Self> {
        Ok(AuthorId(VerifyingKey::from_bytes(bytes)?))
    }
}

/// Namespace key of a [`Replica`].
///
/// Holders of this key can insert new entries into a [`Replica`].
/// Internally, a namespace is a [`SigningKey`] which is used to sign entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Namespace {
    priv_key: SigningKey,
}

impl Namespace {
    /// Create a new namespace with a random key.
    pub fn new<R: CryptoRngCore + ?Sized>(rng: &mut R) -> Self {
        let priv_key = SigningKey::generate(rng);

        Namespace { priv_key }
    }

    /// Create a namespace from a byte array.
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        SigningKey::from_bytes(bytes).into()
    }

    /// Returns the namespace byte representation.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.priv_key.to_bytes()
    }

    /// Returns the [`NamespaceId`] byte representation.
    pub fn id_bytes(&self) -> [u8; 32] {
        self.priv_key.verifying_key().to_bytes()
    }

    /// Get the [`NamespaceId`] for this namespace.
    pub fn id(&self) -> NamespaceId {
        NamespaceId(self.priv_key.verifying_key())
    }

    /// Sign a message with this namespace key.
    pub fn sign(&self, msg: &[u8]) -> Signature {
        self.priv_key.sign(msg)
    }

    /// Strictly verify a signature on a message with this namespaces's public key.
    pub fn verify(&self, msg: &[u8], signature: &Signature) -> Result<(), SignatureError> {
        self.priv_key.verify_strict(msg, signature)
    }
}

/// Identifier for a [`Namespace`]
///
/// This is the corresponding [`VerifyingKey`] for an author. It is used as an identifier, and can
/// be used to verify [`Signature`]s.
#[derive(Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct NamespaceId(VerifyingKey);

impl NamespaceId {
    /// Verify that a signature matches the `msg` bytes and was created with this the [`Author`]
    /// that corresponds to this [`NamespaceId`].
    pub fn verify(&self, msg: &[u8], signature: &Signature) -> Result<(), SignatureError> {
        self.0.verify_strict(msg, signature)
    }

    /// Get the byte representation of this [`NamespaceId`].
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Construct a `NamespaceId` from a slice of bytes.
    ///
    /// # Warning
    ///
    /// The caller is responsible for ensuring that the bytes passed into this method actually
    /// represent a valid [`ed25591`] curve point. This will never fail for bytes returned from
    /// [`Self::as_bytes`].
    pub fn from_bytes(bytes: &[u8; 32]) -> anyhow::Result<Self> {
        Ok(NamespaceId(VerifyingKey::from_bytes(bytes)?))
    }
}

impl fmt::Display for Author {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Author({})", hex::encode(self.priv_key.to_bytes()))
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Namespace({})", hex::encode(self.priv_key.to_bytes()))
    }
}

impl fmt::Display for AuthorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0.as_bytes()))
    }
}

impl fmt::Display for NamespaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0.as_bytes()))
    }
}

impl fmt::Debug for NamespaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NamespaceId({})", hex::encode(self.0.as_bytes()))
    }
}

impl fmt::Debug for AuthorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AuthorId({})", hex::encode(self.0.as_bytes()))
    }
}

impl FromStr for Author {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let priv_key: [u8; 32] = hex::decode(s).map_err(|_| ())?.try_into().map_err(|_| ())?;
        let priv_key = SigningKey::from_bytes(&priv_key);

        Ok(Author { priv_key })
    }
}

impl FromStr for Namespace {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let priv_key: [u8; 32] = hex::decode(s).map_err(|_| ())?.try_into().map_err(|_| ())?;
        let priv_key = SigningKey::from_bytes(&priv_key);

        Ok(Namespace { priv_key })
    }
}

impl FromStr for AuthorId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let pub_key: [u8; 32] = hex::decode(s)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("failed to parse: invalid key length"))?;
        let pub_key = VerifyingKey::from_bytes(&pub_key)?;
        Ok(AuthorId(pub_key))
    }
}

impl FromStr for NamespaceId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let pub_key: [u8; 32] = hex::decode(s)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("failed to parse: invalid key length"))?;
        let pub_key = VerifyingKey::from_bytes(&pub_key)?;
        Ok(NamespaceId(pub_key))
    }
}

impl From<SigningKey> for Author {
    fn from(priv_key: SigningKey) -> Self {
        Self { priv_key }
    }
}

impl From<SigningKey> for Namespace {
    fn from(priv_key: SigningKey) -> Self {
        Self { priv_key }
    }
}

impl PartialOrd for NamespaceId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NamespaceId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_bytes().cmp(other.0.as_bytes())
    }
}

impl PartialOrd for AuthorId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AuthorId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_bytes().cmp(other.0.as_bytes())
    }
}
