//! Minimal RESP2 encoder / reply parser: just what GET and SET need.

use crate::values::{fmt_u64, ValueGen};

#[inline]
fn push_len(out: &mut Vec<u8>, tag: u8, n: usize) {
    let mut b = [0u8; 20];
    out.push(tag);
    out.extend_from_slice(fmt_u64(n as u64, &mut b));
    out.extend_from_slice(b"\r\n");
}

#[inline]
fn push_key(out: &mut Vec<u8>, prefix: &[u8], key: u64) {
    let mut b = [0u8; 20];
    let k = fmt_u64(key, &mut b);
    push_len(out, b'$', prefix.len() + k.len());
    out.extend_from_slice(prefix);
    out.extend_from_slice(k);
    out.extend_from_slice(b"\r\n");
}

#[inline]
pub fn encode_get(out: &mut Vec<u8>, prefix: &[u8], key: u64) {
    out.extend_from_slice(b"*2\r\n$3\r\nGET\r\n");
    push_key(out, prefix, key);
}

#[inline]
pub fn encode_set(out: &mut Vec<u8>, prefix: &[u8], key: u64, ver: u64, vals: &ValueGen) -> usize {
    out.extend_from_slice(b"*3\r\n$3\r\nSET\r\n");
    push_key(out, prefix, key);
    let len = vals.len(key, ver);
    push_len(out, b'$', len);
    vals.write(key, ver, out);
    out.extend_from_slice(b"\r\n");
    len
}

/// SET with a value that is already prepared (e.g. compressed by the client).
#[inline]
pub fn encode_set_bytes(out: &mut Vec<u8>, prefix: &[u8], key: u64, val: &[u8]) {
    out.extend_from_slice(b"*3\r\n$3\r\nSET\r\n");
    push_key(out, prefix, key);
    push_len(out, b'$', val.len());
    out.extend_from_slice(val);
    out.extend_from_slice(b"\r\n");
}

pub fn encode_cmd(out: &mut Vec<u8>, args: &[&[u8]]) {
    push_len(out, b'*', args.len());
    for a in args {
        push_len(out, b'$', a.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
}

#[derive(Debug, PartialEq)]
pub enum Reply {
    Ok,           // +...
    Error,        // -...
    Int,          // :...
    Nil,          // $-1 / _
    Bulk(usize, usize), // start, len within the buffer
}

/// Try to parse one reply from `buf[pos..]`. Returns (reply, bytes consumed)
/// or None if more data is needed. Aggregates are not expected for GET/SET.
#[inline]
pub fn parse(buf: &[u8], pos: usize) -> Result<Option<(Reply, usize)>, String> {
    let b = &buf[pos..];
    if b.is_empty() {
        return Ok(None);
    }
    let line_end = match memchr_crlf(b) {
        Some(i) => i,
        None => return Ok(None),
    };
    match b[0] {
        b'+' => Ok(Some((Reply::Ok, line_end + 2))),
        b'-' => Ok(Some((Reply::Error, line_end + 2))),
        b':' => Ok(Some((Reply::Int, line_end + 2))),
        b'_' => Ok(Some((Reply::Nil, line_end + 2))),
        b'$' => {
            if b[1] == b'-' {
                return Ok(Some((Reply::Nil, line_end + 2)));
            }
            let mut n = 0usize;
            for &c in &b[1..line_end] {
                if !c.is_ascii_digit() {
                    return Err(format!("bad bulk length {:?}", String::from_utf8_lossy(&b[..line_end])));
                }
                n = n * 10 + (c - b'0') as usize;
            }
            let need = line_end + 2 + n + 2;
            if b.len() < need {
                return Ok(None);
            }
            Ok(Some((Reply::Bulk(pos + line_end + 2, n), need)))
        }
        c => Err(format!("unexpected reply type byte {:?}", c as char)),
    }
}

#[inline]
fn memchr_crlf(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'\r' && b[i + 1] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_replies() {
        let buf = b"+OK\r\n$5\r\nhello\r\n$-1\r\n-ERR x\r\n$3\r\nab";
        let (r, n) = parse(buf, 0).unwrap().unwrap();
        assert_eq!((r, n), (Reply::Ok, 5));
        let (r, n2) = parse(buf, 5).unwrap().unwrap();
        assert_eq!(r, Reply::Bulk(9, 5));
        assert_eq!(&buf[9..14], b"hello");
        let (r, n3) = parse(buf, 5 + n2).unwrap().unwrap();
        assert_eq!(r, Reply::Nil);
        let (r, n4) = parse(buf, 5 + n2 + n3).unwrap().unwrap();
        assert_eq!(r, Reply::Error);
        assert!(parse(buf, 5 + n2 + n3 + n4).unwrap().is_none());
    }

    #[test]
    fn encode() {
        let mut o = Vec::new();
        encode_get(&mut o, b"k:", 42);
        assert_eq!(o, b"*2\r\n$3\r\nGET\r\n$4\r\nk:42\r\n");
    }
}
