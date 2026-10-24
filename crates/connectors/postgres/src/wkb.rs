//! PostGIS `geometry` / `geography` values arrive as hex EWKB
//! (`0101000020E6…`). Shown and exported as EWKT instead
//! (`SRID=4326;POINT(1 2)`, what `ST_AsEWKT` writes; readable by
//! `ST_GeomFromEWKT` and most GIS tools).

/// EWKT of a hex (E)WKB value; `None` when it isn't one (or is a curve type
/// this doesn't decode), so the caller keeps the original text.
pub fn hex_ewkb_to_ewkt(hex: &str) -> Option<String> {
    if hex.len() < 10 || hex.len() % 2 != 0 {
        return None;
    }
    let bytes: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect::<Option<_>>()?;
    let mut r = Reader { b: &bytes, i: 0, le: true };
    let mut out = String::new();
    geometry(&mut r, &mut out, true)?;
    (r.i == bytes.len()).then_some(out)
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
    le: bool,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let s: [u8; N] = self.b.get(self.i..self.i + N)?.try_into().ok()?;
        self.i += N;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.take::<4>()?;
        Some(if self.le { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) })
    }
    fn f64(&mut self) -> Option<f64> {
        let s = self.take::<8>()?;
        Some(if self.le { f64::from_le_bytes(s) } else { f64::from_be_bytes(s) })
    }
}

fn geometry(r: &mut Reader, out: &mut String, top: bool) -> Option<()> {
    r.le = match r.take::<1>()?[0] {
        0 => false,
        1 => true,
        _ => return None,
    };
    let raw = r.u32()?;
    // EWKB flags, or ISO WKB type codes (1001 = Point Z, 2001 = Point M, 3001 = ZM).
    let (mut z, mut m) = (raw & 0x8000_0000 != 0, raw & 0x4000_0000 != 0);
    let has_srid = raw & 0x2000_0000 != 0;
    let mut kind = raw & 0x0FFF_FFFF;
    match kind / 1000 {
        1 => z = true,
        2 => m = true,
        3 => (z, m) = (true, true),
        _ => {}
    }
    kind %= 1000;
    if has_srid {
        let srid = r.u32()?;
        if top && srid != 0 {
            out.push_str(&format!("SRID={srid};"));
        }
    }
    let dims = 2 + z as usize + m as usize;
    let name = match kind {
        1 => "POINT",
        2 => "LINESTRING",
        3 => "POLYGON",
        4 => "MULTIPOINT",
        5 => "MULTILINESTRING",
        6 => "MULTIPOLYGON",
        7 => "GEOMETRYCOLLECTION",
        _ => return None,
    };
    out.push_str(name);
    if m && !z {
        out.push('M');
    }
    match kind {
        1 => {
            let c: Vec<f64> = (0..dims).map(|_| r.f64()).collect::<Option<_>>()?;
            if c.iter().all(|v| v.is_nan()) {
                out.push_str(" EMPTY");
            } else {
                out.push('(');
                coords(out, &c);
                out.push(')');
            }
        }
        2 => points(r, out, dims)?,
        3 => rings(r, out, dims)?,
        _ => {
            let n = r.u32()?;
            if n == 0 {
                out.push_str(" EMPTY");
                return Some(());
            }
            out.push('(');
            for k in 0..n {
                if k > 0 {
                    out.push(',');
                }
                // Members of MULTI* are written without their type name.
                let mut part = String::new();
                geometry(r, &mut part, false)?;
                if kind == 7 {
                    out.push_str(&part);
                } else {
                    out.push_str(part.trim_start_matches(|c: char| c.is_ascii_alphabetic()));
                }
            }
            out.push(')');
        }
    }
    Some(())
}

fn coords(out: &mut String, c: &[f64]) {
    for (k, v) in c.iter().enumerate() {
        if k > 0 {
            out.push(' ');
        }
        out.push_str(&v.to_string());
    }
}

fn points(r: &mut Reader, out: &mut String, dims: usize) -> Option<()> {
    let n = r.u32()?;
    if n == 0 {
        out.push_str(" EMPTY");
        return Some(());
    }
    out.push('(');
    for k in 0..n {
        if k > 0 {
            out.push(',');
        }
        let c: Vec<f64> = (0..dims).map(|_| r.f64()).collect::<Option<_>>()?;
        coords(out, &c);
    }
    out.push(')');
    Some(())
}

fn rings(r: &mut Reader, out: &mut String, dims: usize) -> Option<()> {
    let n = r.u32()?;
    if n == 0 {
        out.push_str(" EMPTY");
        return Some(());
    }
    out.push('(');
    for k in 0..n {
        if k > 0 {
            out.push(',');
        }
        points(r, out, dims)?;
    }
    out.push(')');
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Little-endian WKB: `parts` are u32 counts (as `C(n)`) and coordinates.
    enum P {
        C(u32),
        F(f64),
        G(u32),
    }
    fn hex(parts: &[P]) -> String {
        let mut b = Vec::new();
        for p in parts {
            match p {
                P::C(n) => b.extend(n.to_le_bytes()),
                P::F(f) => b.extend(f.to_le_bytes()),
                P::G(t) => {
                    b.push(1);
                    b.extend(t.to_le_bytes());
                }
            }
        }
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    #[test]
    fn decodes_postgis_values() {
        use P::*;
        // SELECT 'SRID=4326;POINT(1 2)'::geometry (as PostGIS sends it)
        assert_eq!(hex_ewkb_to_ewkt("0101000020E6100000000000000000F03F0000000000000040").as_deref(), Some("SRID=4326;POINT(1 2)"));
        assert_eq!(hex_ewkb_to_ewkt(&hex(&[G(2), C(2), F(0.0), F(0.0), F(1.0), F(1.5)])).as_deref(), Some("LINESTRING(0 0,1 1.5)"));
        assert_eq!(
            hex_ewkb_to_ewkt(&hex(&[G(3), C(1), C(4), F(0.0), F(0.0), F(1.0), F(0.0), F(1.0), F(1.0), F(0.0), F(0.0)])).as_deref(),
            Some("POLYGON((0 0,1 0,1 1,0 0))")
        );
        assert_eq!(hex_ewkb_to_ewkt(&hex(&[G(4), C(2), G(1), F(1.0), F(2.0), G(1), F(3.0), F(4.0)])).as_deref(), Some("MULTIPOINT((1 2),(3 4))"));
        assert_eq!(
            hex_ewkb_to_ewkt(&hex(&[G(7), C(2), G(1), F(1.0), F(2.0), G(2), C(2), F(0.0), F(0.0), F(1.0), F(1.0)])).as_deref(),
            Some("GEOMETRYCOLLECTION(POINT(1 2),LINESTRING(0 0,1 1))")
        );
        // EWKB Z flag, ISO Z code, M only, big-endian, empty point.
        assert_eq!(hex_ewkb_to_ewkt(&hex(&[G(0x8000_0001), F(1.0), F(2.0), F(3.0)])).as_deref(), Some("POINT(1 2 3)"));
        assert_eq!(hex_ewkb_to_ewkt(&hex(&[G(1001), F(1.0), F(2.0), F(3.0)])).as_deref(), Some("POINT(1 2 3)"));
        assert_eq!(hex_ewkb_to_ewkt(&hex(&[G(0x4000_0001), F(1.0), F(2.0), F(3.0)])).as_deref(), Some("POINTM(1 2 3)"));
        assert_eq!(hex_ewkb_to_ewkt("00000000013FF00000000000004000000000000000").as_deref(), Some("POINT(1 2)"));
        assert_eq!(hex_ewkb_to_ewkt(&hex(&[G(1), F(f64::NAN), F(f64::NAN)])).as_deref(), Some("POINT EMPTY"));
        // Not EWKB: left alone.
        assert_eq!(hex_ewkb_to_ewkt("hello"), None);
        assert_eq!(hex_ewkb_to_ewkt("0101000000000000000000F03F"), None, "truncated");
        assert_eq!(hex_ewkb_to_ewkt(&hex(&[G(8), C(0)])), None, "curves stay hex");
    }
}
