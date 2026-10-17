//! The chain API request parsing, error bodies and block renderer against what Spring nodes
//! answered (`testdata/chain-api`, whose README says how it was captured), byte for byte:
//!
//!   * every request of `dev-chain/requests` (a Spring 1.2.2 dev chain: the error matrix and
//!     each endpoint for blocks from genesis through the Savanna switch) is parsed as the server
//!     parses it, answered from the blocks of `dev-chain/blocks` (or as Unknown Block), and the
//!     status and the body must be nodeos's;
//!   * EOS mainnet block 480385480 (`testdata/blocks`) renders to the four bodies public Spring
//!     nodes answer for it.

#![cfg(feature = "chain-api")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use hyperion_ship::chainapi::block::{Compression, ReceiptTrx, Signature};
use hyperion_ship::chainapi::{Endpoint, parse, parse_signed_block, render};

fn goldens() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/chain-api")
}

/// The blocks of `dev-chain/blocks`, by number.
fn dev_blocks() -> BTreeMap<u32, Vec<u8>> {
    let dir = goldens().join("dev-chain/blocks");
    let mut blocks = BTreeMap::new();
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let n: u32 = path.file_stem().unwrap().to_str().unwrap().parse().unwrap();
        let bytes = hex::decode(fs::read_to_string(&path).unwrap().trim()).unwrap();
        let b = parse_signed_block(&bytes).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        assert_eq!(b.number, n, "{}", path.display());
        blocks.insert(n, bytes);
    }
    blocks
}

/// One captured exchange: the request's target and body, nodeos's status and body.
struct Exchange {
    name: String,
    target: String,
    body: Vec<u8>,
    status: u16,
    answer: Vec<u8>,
}

/// The head lines of an HTTP message and what follows them.
fn split(message: &[u8]) -> (Vec<String>, &[u8]) {
    let at = message
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a head");
    let head = std::str::from_utf8(&message[..at]).expect("a text head");
    (
        head.split("\r\n").map(str::to_owned).collect(),
        &message[at + 4..],
    )
}

/// The body a head's `Content-Length` frames (none without one).
fn framed<'a>(head: &[String], rest: &'a [u8]) -> &'a [u8] {
    let len = head[1..]
        .iter()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    &rest[..len]
}

/// The exchanges of a directory of `<case>.req` and `<case>.resp` pairs, by case name.
fn exchanges(dir: &Path) -> Vec<Exchange> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "req") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        let request = fs::read(&path).unwrap();
        let (head, rest) = split(&request);
        let target = head[0]
            .split(' ')
            .nth(1)
            .expect("a request line")
            .to_owned();
        let body = framed(&head, rest).to_vec();
        let response = fs::read(path.with_extension("resp")).unwrap();
        let (head, rest) = split(&response);
        let status = head[0].split(' ').nth(1).unwrap().parse().unwrap();
        let answer = framed(&head, rest).to_vec();
        out.push(Exchange {
            name,
            target,
            body,
            status,
            answer,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// What a server holding `blocks` answers `endpoint` asked with `body`: the status and body.
fn answer(endpoint: Endpoint, body: &[u8], blocks: &BTreeMap<u32, Vec<u8>>) -> (u16, Vec<u8>) {
    let req = match parse(endpoint, body) {
        Ok(req) => req,
        Err(e) => return (e.status(), e.body()),
    };
    let block = blocks
        .get(&req.block.num())
        .map(|bytes| parse_signed_block(bytes).unwrap())
        .filter(|b| req.block.matches(&b.id));
    match block {
        Some(b) => {
            let mut out = Vec::new();
            render(endpoint, &b, req.include_extensions, usize::MAX, &mut out).unwrap();
            (200, out)
        }
        None => {
            let e = req.unknown_block();
            (e.status(), e.body())
        }
    }
}

#[test]
fn every_dev_chain_request_is_answered_as_nodeos_answered_it() {
    let blocks = dev_blocks();
    let all = exchanges(&goldens().join("dev-chain/requests"));
    let mut by_status = BTreeMap::<u16, usize>::new();
    let mut wrong = Vec::new();
    for x in &all {
        let endpoint = Endpoint::from_target(&x.target)
            .unwrap_or_else(|| panic!("{}: {} is not a block endpoint", x.name, x.target));
        let (status, body) = answer(endpoint, &x.body, &blocks);
        if (status, &body) != (x.status, &x.answer) {
            wrong.push(format!(
                "{}: {} {}\n  nodeos: {}\n  here:   {}",
                x.name,
                status,
                String::from_utf8_lossy(&x.body),
                String::from_utf8_lossy(&x.answer),
                String::from_utf8_lossy(&body)
            ));
        }
        *by_status.entry(x.status).or_default() += 1;
    }
    assert!(
        wrong.is_empty(),
        "{} differ:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    // Every status of the catalogue, and every captured block in every mode.
    assert_eq!(
        by_status.keys().copied().collect::<Vec<_>>(),
        [200, 400, 500]
    );
    for n in blocks.keys() {
        for case in [
            "info",
            "header",
            "header-ext",
            "raw",
            "header-ext-id",
            "raw-id",
        ] {
            let name = format!("block-{n}-{case}");
            let x = all.iter().find(|x| x.name == name);
            assert_eq!(x.map(|x| x.status), Some(200), "{name}");
        }
    }
    assert!(all.len() > 200, "{} exchanges", all.len());
}

#[test]
fn the_length_assert_line_numbers_are_pinned_by_nodeos() {
    // The constants are the last line of each multi-line assert, as nodeos reports it.
    let all = exchanges(&goldens().join("dev-chain/requests"));
    for (case, method, line) in [
        ("raw-len-empty", "get_raw_block", 1952),
        ("raw-len-65", "get_raw_block", 1952),
        ("hdr0-len-empty", "get_block_header", 1990),
        ("hdr1-len-65", "get_block_header", 1990),
    ] {
        let x = all.iter().find(|x| x.name == case).expect(case);
        let text = String::from_utf8(x.answer.clone()).unwrap();
        let want = format!("\"line_number\":{line},\"method\":\"{method}\"");
        assert!(text.contains(&want), "{case}: {text}");
    }
    assert_eq!(hyperion_ship::chainapi::errors::L_RAW, 1952);
    assert_eq!(hyperion_ship::chainapi::errors::L_HDR, 1990);
}

#[test]
fn the_dev_chain_blocks_hold_what_the_goldens_are_kept_for() {
    let mut header_extensions = BTreeMap::<u16, usize>::new();
    let mut block_extensions = BTreeMap::<u16, usize>::new();
    let (mut zlib, mut context_free, mut r1, mut several, mut receipts) = (0, 0, 0, 0, 0);
    for bytes in dev_blocks().values() {
        let b = parse_signed_block(bytes).unwrap();
        for e in b.header_extensions.iter() {
            *header_extensions.entry(e.unwrap().kind).or_default() += 1;
        }
        for e in b.block_extensions.iter() {
            *block_extensions.entry(e.unwrap().kind).or_default() += 1;
        }
        for r in b.transactions.iter() {
            let ReceiptTrx::Packed(p) = r.unwrap().trx else {
                panic!("block {}: an id-only receipt", b.number)
            };
            receipts += 1;
            zlib += usize::from(p.compression == Compression::Zlib);
            context_free += usize::from(!p.packed_context_free_data.is_empty());
            several += usize::from(p.signatures.len() > 1);
            r1 += p
                .signatures
                .iter()
                .filter(|s| matches!(s, Ok(Signature::R1(_))))
                .count();
        }
    }
    // Feature activations (0), a producer schedule change (1), finality data (2); QCs (3).
    assert_eq!(
        header_extensions.keys().copied().collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(block_extensions.keys().copied().collect::<Vec<_>>(), [3]);
    assert!(receipts > 20, "{receipts} receipts");
    for (what, count) in [
        ("zlib", zlib),
        ("context-free data", context_free),
        ("R1 signatures", r1),
        ("several signatures", several),
    ] {
        assert!(count > 0, "no receipt with {what}");
    }
}

#[test]
fn a_public_block_renders_as_spring_nodes_render_it() {
    let hex = include_str!("../testdata/blocks/eos-mainnet-480385480.hex");
    let bytes = hex::decode(hex.trim()).unwrap();
    let b = parse_signed_block(&bytes).unwrap();
    assert_eq!(b.number, 480_385_480);
    let dir = goldens().join("eos-mainnet/480385480");
    for (file, endpoint, include_extensions) in [
        ("get_block_info.json", Endpoint::GetBlockInfo, false),
        ("get_block_header.json", Endpoint::GetBlockHeader, false),
        (
            "get_block_header-extensions.json",
            Endpoint::GetBlockHeader,
            true,
        ),
        ("get_raw_block.json", Endpoint::GetRawBlock, false),
    ] {
        let want = fs::read(dir.join(file)).unwrap();
        let mut out = Vec::new();
        render(endpoint, &b, include_extensions, usize::MAX, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            String::from_utf8(want).unwrap(),
            "{file}"
        );
    }
}
