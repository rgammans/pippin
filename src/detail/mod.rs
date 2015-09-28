//! Pippin implementation details.

//! Many code forms shamelessly lifted from Alex Crichton's flate2 library.

mod sum;

pub use self::read::read_head;

// Information stored in a file header
pub struct FileHeader {
    /// Repo name
    pub name: String,
}

mod read {
    use std::{io, mem, iter};
    use ::detail::{FileHeader, sum};
    use ::error::{Result, Error};
    
    /// Read a file header.
    /// 
    /// Note that if the repo name is not valid UTF-8, conversion is lossy.
    pub fn read_head(r: &mut io::Read) -> Result<FileHeader> {
        // A reader which also calculates a checksum:
        let mut sum_reader = sum::HashReader::new256(r);
        
        let mut buf = [0u8; 16];
        let mut pos: usize = 0;
        try!(fill(&mut sum_reader, &mut buf, pos));
        if buf != *b"PIPPINSS20150924" {
            return Err(Error::read("not a known Pippin file format", pos));
        }
        pos += buf.len();
        
        try!(fill(&mut sum_reader, &mut buf, pos));
        let repo_name = match String::from_utf8(buf.to_vec()) {
            Ok(name) => name,
            Err(_) => return Err(Error::read("repo name not valid UTF-8", pos))
        };
        pos += buf.len();
        
        loop {
            try!(fill(&mut sum_reader, &mut buf, pos));
            if buf[0] == b'H'{
                if buf[0..4] == *b"HSUM" {
                    match &buf[4..] {
                        b" SHA-2 256  " => { /* we don't support anything else */ },
                        _ => return Err(Error::read("unknown checksum format", pos))
                    };
                    break;      // "HSUM" must be last item of header before final checksum
                }
                // else: ignore
            } else if buf[0] == b'Q' {
                let x: usize = match buf[1] {
                    b'1' ... b'9' => buf[1] - b'0',
                    b'A' ... b'Z' => buf[1] + 10 - b'A',
                    _ => return Err(Error::read("header section Qx... has invalid length specification 'x'", pos))
                } as usize;
                let mut qbuf: Vec<u8> = Vec::new();
                qbuf.reserve_exact(16 * x);
                qbuf.extend(&buf);
                qbuf.extend(iter::repeat(0).take(16 * (x-1)));
                try!(fill(&mut sum_reader, &mut qbuf[16..], pos));
                //TODO: match against rules
            } else {
                return Err(Error::read("unexpected header contents", pos));
            }
            pos += buf.len();
        }
        
        // Read checksum (assume SHA-256)
        let mut buf32 = [0u8; 32];
        try!(fill(&mut sum_reader.inner(), &mut buf32, pos));
        assert_eq!( sum_reader.hash_bytes(), 32 );
        let mut sum32 = [0u8; 32];
        sum_reader.hash_result(&mut sum32);
        if buf32 != sum32 {
            return Err(Error::read("header checksum invalid", pos));
        }
        pos += buf.len();
        
        return Ok(FileHeader{
            name: repo_name
        });
        
        fn fill<R: io::Read>(r: &mut R, mut buf: &mut [u8], pos: usize) -> Result<()> {
            let mut p = pos;
            while buf.len() > 0 {
                match try!(r.read(buf)) {
                    0 => return Err(Error::read("corrupt (file terminates unexpectedly)", p)),
                    n => { buf = &mut mem::replace(&mut buf, &mut [])[n..]; p += n },
                }
            }
            Ok(())
        }
        
    }
    
    #[test]
    fn read_header() {
        // Note: checksum calculated with Python 3:
        // import hashlib
        // hashlib.sha256(b"PIPPINSS20150924...").digest()
        let head = b"PIPPINSS20150924\
                    test repo.......\
                    Hdummy text 1234\
                    Q2more completel\
                    y pointless text\
                    HSUM SHA-2 256  \
                    \xc1n}\xe6\x92\x98\xe5\x8b#\xf3\xb8\xe3b\xa5\xc8\xf9\
                    \xdb6\x17\xe1\xc9\xe6\xf1\xd6\x08\xff\xcccR\xcd\xd6\x93";
        let header = read_head(&mut &head[..]).unwrap();
    }
}
