//! A borrowed walk of a packed `signed_block` in the Spring 1.x layout, fail-closed, with fc
//! `raw::unpack` semantics through [`FcReader`] (non-canonical varuints accepted, optional
//! flags 0 or 1):
//!
//! ```text
//! timestamp u32 | producer u64 | confirmed u16 | previous [32] | transaction_mroot [32]
//! | action_mroot [32] | schedule_version u32
//! | new_producers optional<{version u32, producers vec<{producer_name u64, public_key}>}>
//! | header_extensions vec<(u16, bytes)> | producer_signature signature
//! | transactions vec<{status u8, cpu_usage_us u32, net_usage_words varuint32,
//!     trx variant<0: id [32],
//!                 1: {signatures vec<signature>, compression u8,
//!                     packed_context_free_data bytes, packed_trx bytes}>}>
//! | block_extensions vec<(u16, bytes)>
//! ```
//!
//! [`parse_signed_block`] checks the whole block once (every variant index and enum value, and
//! that the block ends exactly after its extensions) without allocating per element: lists keep
//! their packed bytes and decode again, lazily, as the renderer walks them. The id and number
//! come from [`block_header`], the function the SHiP server derives them with.

use std::marker::PhantomData;

use anyhow::{Context, Result, bail, ensure};

use crate::binio::FcReader;
use crate::blocklog::block_header;

/// A packed `signed_block`, borrowed from its bytes.
#[derive(Clone, Copy, Debug)]
pub struct SignedBlock<'a> {
    /// `previous`'s number plus one.
    pub number: u32,
    /// Spring `block_header::calculate_id`.
    pub id: [u8; 32],
    /// `block_timestamp_type` slot: half-seconds since 2000-01-01.
    pub timestamp: u32,
    pub producer: u64,
    pub confirmed: u16,
    pub previous: &'a [u8; 32],
    pub transaction_mroot: &'a [u8; 32],
    pub action_mroot: &'a [u8; 32],
    pub schedule_version: u32,
    pub new_producers: Option<ProducerSchedule<'a>>,
    pub header_extensions: List<'a, Extension<'a>>,
    pub producer_signature: Signature<'a>,
    pub transactions: List<'a, Receipt<'a>>,
    pub block_extensions: List<'a, Extension<'a>>,
}

/// A legacy `producer_schedule_type`, the header's `new_producers`.
#[derive(Clone, Copy, Debug)]
pub struct ProducerSchedule<'a> {
    pub version: u32,
    pub producers: List<'a, ProducerKey<'a>>,
}

/// A legacy `producer_key`.
#[derive(Clone, Copy, Debug)]
pub struct ProducerKey<'a> {
    pub producer_name: u64,
    pub block_signing_key: PublicKey<'a>,
}

/// A `public_key`: variant 0 K1, 1 R1, 2 WebAuthn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicKey<'a> {
    K1(&'a [u8; 33]),
    R1(&'a [u8; 33]),
    /// The compressed point, the user-presence byte and the relying-party id.
    WebAuthn {
        point: &'a [u8; 33],
        user_presence: u8,
        rpid: &'a [u8],
    },
}

/// A `signature`: variant 0 K1, 1 R1, 2 WebAuthn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signature<'a> {
    K1(&'a [u8; 65]),
    R1(&'a [u8; 65]),
    /// The compact signature, the authenticator data and the client data JSON.
    WebAuthn {
        compact: &'a [u8; 65],
        auth_data: &'a [u8],
        client_json: &'a [u8],
    },
}

/// One `extensions_type` entry: a type and opaque data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extension<'a> {
    pub kind: u16,
    pub data: &'a [u8],
}

/// A `transaction_receipt`.
#[derive(Clone, Copy, Debug)]
pub struct Receipt<'a> {
    pub status: TrxStatus,
    pub cpu_usage_us: u32,
    pub net_usage_words: u32,
    pub trx: ReceiptTrx<'a>,
}

/// A receipt's `trx`: variant 0 is a transaction id, 1 a packed transaction.
#[derive(Clone, Copy, Debug)]
pub enum ReceiptTrx<'a> {
    Id(&'a [u8; 32]),
    Packed(PackedTransaction<'a>),
}

/// A `packed_transaction`, its byte fields as stored (compressed or not).
#[derive(Clone, Copy, Debug)]
pub struct PackedTransaction<'a> {
    pub signatures: List<'a, Signature<'a>>,
    pub compression: Compression,
    pub packed_context_free_data: &'a [u8],
    pub packed_trx: &'a [u8],
}

/// `transaction_receipt_header::status_enum`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrxStatus {
    Executed,
    SoftFail,
    HardFail,
    Delayed,
    Expired,
}

impl TrxStatus {
    /// The enum's reflected name, as nodeos writes it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Executed => "executed",
            Self::SoftFail => "soft_fail",
            Self::HardFail => "hard_fail",
            Self::Delayed => "delayed",
            Self::Expired => "expired",
        }
    }
}

/// `packed_transaction::compression_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Zlib,
}

impl Compression {
    /// The enum's reflected name, as nodeos writes it.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Zlib => "zlib",
        }
    }
}

/// A type decoded from its fc packed form.
pub trait Unpack<'a>: Sized {
    fn unpack(r: &mut FcReader<'a>) -> Result<Self>;
}

/// A packed `vector<T>`, validated: its length and its elements' bytes, decoded again on
/// iteration.
pub struct List<'a, T> {
    len: u32,
    bytes: &'a [u8],
    item: PhantomData<fn() -> T>,
}

// Derived impls would require `T: Clone`/`T: Debug`; the list holds no `T`.
impl<T> Clone for List<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for List<'_, T> {}

impl<T> std::fmt::Debug for List<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("List")
            .field("len", &self.len)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl<'a, T: Unpack<'a>> List<'a, T> {
    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The elements in order. Each was decoded once at parse, so an error here means the
    /// bytes changed under the list, which borrowed bytes cannot.
    pub fn iter(&self) -> impl Iterator<Item = Result<T>> + 'a {
        let mut r = FcReader::new(self.bytes);
        (0..self.len).map(move |_| T::unpack(&mut r))
    }
}

impl<'a, T: Unpack<'a>> Unpack<'a> for List<'a, T> {
    fn unpack(r: &mut FcReader<'a>) -> Result<Self> {
        let len = r.varuint32()?;
        let start = r.pos;
        for i in 0..len {
            T::unpack(r).with_context(|| format!("element {i} of {len}"))?;
        }
        Ok(Self {
            len,
            bytes: &r.b[start..r.pos],
            item: PhantomData,
        })
    }
}

impl<'a> Unpack<'a> for PublicKey<'a> {
    fn unpack(r: &mut FcReader<'a>) -> Result<Self> {
        let at = r.pos;
        Ok(match r.varuint32()? {
            0 => Self::K1(array(r)?),
            1 => Self::R1(array(r)?),
            2 => Self::WebAuthn {
                point: array(r)?,
                user_presence: r.u8()?,
                rpid: bytes(r)?,
            },
            k => bail!("unknown public key type {k} at offset {at}"),
        })
    }
}

impl<'a> Unpack<'a> for Signature<'a> {
    fn unpack(r: &mut FcReader<'a>) -> Result<Self> {
        let at = r.pos;
        Ok(match r.varuint32()? {
            0 => Self::K1(array(r)?),
            1 => Self::R1(array(r)?),
            2 => Self::WebAuthn {
                compact: array(r)?,
                auth_data: bytes(r)?,
                client_json: bytes(r)?,
            },
            k => bail!("unknown signature type {k} at offset {at}"),
        })
    }
}

impl<'a> Unpack<'a> for ProducerKey<'a> {
    fn unpack(r: &mut FcReader<'a>) -> Result<Self> {
        Ok(Self {
            producer_name: r.u64()?,
            block_signing_key: PublicKey::unpack(r)?,
        })
    }
}

impl<'a> Unpack<'a> for Extension<'a> {
    fn unpack(r: &mut FcReader<'a>) -> Result<Self> {
        Ok(Self {
            kind: u16_le(r)?,
            data: bytes(r)?,
        })
    }
}

impl<'a> Unpack<'a> for Receipt<'a> {
    fn unpack(r: &mut FcReader<'a>) -> Result<Self> {
        let at = r.pos;
        let status = match r.u8()? {
            0 => TrxStatus::Executed,
            1 => TrxStatus::SoftFail,
            2 => TrxStatus::HardFail,
            3 => TrxStatus::Delayed,
            4 => TrxStatus::Expired,
            s => bail!("unknown transaction status {s} at offset {at}"),
        };
        let cpu_usage_us = r.u32()?;
        let net_usage_words = r.varuint32()?;
        let at = r.pos;
        let trx = match r.varuint32()? {
            0 => ReceiptTrx::Id(array(r)?),
            1 => ReceiptTrx::Packed(PackedTransaction::unpack(r)?),
            k => bail!("unknown receipt trx type {k} at offset {at}"),
        };
        Ok(Self {
            status,
            cpu_usage_us,
            net_usage_words,
            trx,
        })
    }
}

impl<'a> Unpack<'a> for PackedTransaction<'a> {
    fn unpack(r: &mut FcReader<'a>) -> Result<Self> {
        let signatures = List::unpack(r).context("transaction signatures")?;
        let at = r.pos;
        let compression = match r.u8()? {
            0 => Compression::None,
            1 => Compression::Zlib,
            c => bail!("unknown transaction compression {c} at offset {at}"),
        };
        Ok(Self {
            signatures,
            compression,
            packed_context_free_data: bytes(r)?,
            packed_trx: bytes(r)?,
        })
    }
}

/// Walk the whole of `body`, a packed `signed_block`. Errors on anything nodeos could not
/// have written: a truncated field, an unknown variant index or enum value, or bytes left
/// after the block extensions.
pub fn parse_signed_block(body: &[u8]) -> Result<SignedBlock<'_>> {
    let header = block_header(body).context("block header")?;
    let mut r = FcReader::new(body);
    let timestamp = r.u32()?;
    let producer = r.u64()?;
    let confirmed = u16_le(&mut r)?;
    let previous: &[u8; 32] = array(&mut r)?;
    ensure!(
        *previous == header.previous,
        "block header previous mismatch"
    );
    let transaction_mroot = array(&mut r)?;
    let action_mroot = array(&mut r)?;
    let schedule_version = r.u32()?;
    let new_producers = if r.bool().context("new_producers flag")? {
        Some(ProducerSchedule {
            version: r.u32()?,
            producers: List::unpack(&mut r).context("new_producers")?,
        })
    } else {
        None
    };
    let header_extensions = List::unpack(&mut r).context("header_extensions")?;
    let producer_signature = Signature::unpack(&mut r).context("producer_signature")?;
    let transactions = List::unpack(&mut r).context("transactions")?;
    let block_extensions = List::unpack(&mut r).context("block_extensions")?;
    ensure!(
        r.remaining() == 0,
        "{} bytes after the block extensions of block {} ({} bytes)",
        r.remaining(),
        header.number,
        body.len()
    );
    Ok(SignedBlock {
        number: header.number,
        id: header.id,
        timestamp,
        producer,
        confirmed,
        previous,
        transaction_mroot,
        action_mroot,
        schedule_version,
        new_producers,
        header_extensions,
        producer_signature,
        transactions,
        block_extensions,
    })
}

/// fc `bytes` (and `string`): a varuint32 length, then the data.
fn bytes<'a>(r: &mut FcReader<'a>) -> Result<&'a [u8]> {
    let len = r.varuint32()?;
    r.bytes(usize::try_from(len)?)
}

fn array<'a, const N: usize>(r: &mut FcReader<'a>) -> Result<&'a [u8; N]> {
    Ok(r.bytes(N)?.try_into()?)
}

fn u16_le(r: &mut FcReader<'_>) -> Result<u16> {
    Ok(u16::from_le_bytes(*array(r)?))
}

/// The fixtures come from the testkit writers.
#[cfg(all(test, feature = "testkit"))]
mod tests {
    use super::*;
    use crate::testkit::blocks::{
        self, Block, BlockSpec, Key, Receipt as R, ReceiptTrx as T, Signature as S, synthetic_id,
    };

    fn err(body: &[u8]) -> String {
        format!("{:#}", parse_signed_block(body).unwrap_err())
    }

    fn receipt(status: u8, trx: T) -> R {
        R {
            status,
            cpu_usage_us: 100 + u32::from(status),
            net_usage_words: 300,
            trx,
        }
    }

    fn packed(compression: u8) -> T {
        T::Packed {
            signatures: vec![S::K1([1; 65]), S::R1([2; 65])],
            compression,
            packed_context_free_data: vec![],
            packed_trx: vec![0xab; 40],
        }
    }

    fn full_spec() -> BlockSpec {
        BlockSpec {
            timestamp: 9,
            producer: blocks::EOSIO,
            confirmed: 3,
            transaction_mroot: [0x11; 32],
            action_mroot: [0x22; 32],
            schedule_version: 4,
            new_producers: Some((
                5,
                vec![
                    (1, Key::K1([0x02; 33])),
                    (2, Key::R1([0x03; 33])),
                    (
                        3,
                        Key::WebAuthn {
                            point: [0x02; 33],
                            user_presence: 1,
                            rpid: "example.com".into(),
                        },
                    ),
                ],
            )),
            header_extensions: vec![(0, vec![0xaa; 4])],
            block_extensions: vec![(2, vec![0xbb; 3]), (3, vec![])],
            producer_signature: S::WebAuthn {
                compact: [7; 65],
                auth_data: vec![8; 37],
                client_json: "{}".into(),
            },
            transactions: vec![
                receipt(0, T::Id([0x33; 32])),
                receipt(1, packed(0)),
                receipt(4, packed(1)),
            ],
        }
    }

    #[test]
    fn every_field_is_read_back() {
        let previous = synthetic_id(41);
        let block = Block::new(&previous, &full_spec()).unwrap();
        let b = parse_signed_block(&block.body).unwrap();
        assert_eq!((b.number, b.id, *b.previous), (42, block.id, previous));
        assert_eq!(
            (b.timestamp, b.producer, b.confirmed),
            (9, blocks::EOSIO, 3)
        );
        assert_eq!(
            (*b.transaction_mroot, *b.action_mroot),
            ([0x11; 32], [0x22; 32])
        );
        assert_eq!(b.schedule_version, 4);

        let np = b.new_producers.unwrap();
        assert_eq!(np.version, 5);
        let keys: Vec<_> = np.producers.iter().map(Result::unwrap).collect();
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].producer_name, 1);
        assert_eq!(keys[0].block_signing_key, PublicKey::K1(&[0x02; 33]));
        assert_eq!(keys[1].block_signing_key, PublicKey::R1(&[0x03; 33]));
        assert_eq!(
            keys[2].block_signing_key,
            PublicKey::WebAuthn {
                point: &[0x02; 33],
                user_presence: 1,
                rpid: b"example.com"
            }
        );

        let ext: Vec<_> = b.header_extensions.iter().map(Result::unwrap).collect();
        assert_eq!(
            ext,
            [Extension {
                kind: 0,
                data: &[0xaa; 4]
            }]
        );
        let ext: Vec<_> = b.block_extensions.iter().map(Result::unwrap).collect();
        assert_eq!(
            ext,
            [
                Extension {
                    kind: 2,
                    data: &[0xbb; 3]
                },
                Extension { kind: 3, data: &[] }
            ]
        );
        assert_eq!(
            b.producer_signature,
            Signature::WebAuthn {
                compact: &[7; 65],
                auth_data: &[8; 37],
                client_json: b"{}"
            }
        );

        let trx: Vec<_> = b.transactions.iter().map(Result::unwrap).collect();
        assert_eq!(trx.len(), 3);
        assert_eq!(trx[0].status, TrxStatus::Executed);
        assert!(matches!(trx[0].trx, ReceiptTrx::Id(id) if *id == [0x33; 32]));
        assert_eq!(
            (trx[1].status, trx[1].cpu_usage_us),
            (TrxStatus::SoftFail, 101)
        );
        assert_eq!(trx[1].net_usage_words, 300);
        let ReceiptTrx::Packed(p) = trx[2].trx else {
            panic!("packed receipt")
        };
        assert_eq!(trx[2].status, TrxStatus::Expired);
        assert_eq!(p.compression, Compression::Zlib);
        assert_eq!(
            (p.packed_context_free_data, p.packed_trx),
            (&[][..], &[0xab; 40][..])
        );
        let sigs: Vec<_> = p.signatures.iter().map(Result::unwrap).collect();
        assert_eq!(sigs, [Signature::K1(&[1; 65]), Signature::R1(&[2; 65])]);
    }

    #[test]
    fn the_default_synthetic_block_parses() {
        let block = Block::new(&synthetic_id(6), &BlockSpec::synthetic(7)).unwrap();
        let b = parse_signed_block(&block.body).unwrap();
        assert_eq!(b.number, 7);
        assert!(b.new_producers.is_none() && b.transactions.is_empty());
        assert_eq!(b.producer_signature, Signature::K1(&[0; 65]));
        assert_eq!(b.block_extensions.len(), 1);
    }

    #[test]
    fn non_canonical_varuints_are_accepted() {
        // One receipt: status, cpu, net_usage_words 5 as a padded varuint, an id.
        let spec = BlockSpec::default();
        let previous = synthetic_id(1);
        let mut body = spec.header(&previous);
        S::default().pack_into(&mut body);
        body.extend_from_slice(&[0x81, 0x00]); // one receipt, padded count
        body.push(0);
        body.extend_from_slice(&7u32.to_le_bytes());
        body.extend_from_slice(&[0x85, 0x80, 0x00]);
        body.push(0);
        body.extend_from_slice(&[9; 32]);
        body.push(0);
        let b = parse_signed_block(&body).unwrap();
        let r: Vec<_> = b.transactions.iter().map(Result::unwrap).collect();
        assert_eq!(
            (r.len(), r[0].cpu_usage_us, r[0].net_usage_words),
            (1, 7, 5)
        );
    }

    #[test]
    fn damaged_blocks_are_refused() {
        let previous = synthetic_id(41);
        let body = Block::new(&previous, &full_spec()).unwrap().body;

        let mut trailing = body.clone();
        trailing.push(0);
        assert!(err(&trailing).contains("1 bytes after the block extensions"));
        assert!(parse_signed_block(&body[..body.len() - 1]).is_err());

        let with = |status: u8, trx: T| {
            let spec = BlockSpec {
                transactions: vec![receipt(status, trx)],
                ..BlockSpec::default()
            };
            Block::new(&previous, &spec).unwrap().body
        };
        assert!(err(&with(5, T::Id([0; 32]))).contains("unknown transaction status 5"));
        assert!(err(&with(0, packed(2))).contains("unknown transaction compression 2"));

        // The trx index is the byte before the 32-byte id, which the empty block extensions
        // (one count byte) follow.
        let mut bad_variant = with(0, T::Id([0; 32]));
        let at = bad_variant.len() - 1 - 32 - 1;
        assert_eq!(bad_variant[at], 0);
        bad_variant[at] = 2;
        assert!(err(&bad_variant).contains("unknown receipt trx type 2"));

        // The producer signature's type precedes its 65 bytes and the two empty counts.
        let mut bad_sig = Block::new(&previous, &BlockSpec::default()).unwrap().body;
        let at = bad_sig.len() - 1 - 1 - 65 - 1;
        assert_eq!(bad_sig[at], 0);
        bad_sig[at] = 3;
        assert!(err(&bad_sig).contains("unknown signature type 3"));
    }
}
