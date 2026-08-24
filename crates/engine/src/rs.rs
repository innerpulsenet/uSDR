//! Shortened Reed-Solomon over GF(256), Phil Karn / dump978 parameters.
//!
//! UAT uses `gfpoly=0x187`, `fcr=120`, `prim=1`. Short ADS-B frames are
//! RS(30,18) (`nroots=12`, `pad=225`); long frames are RS(48,34)
//! (`nroots=14`, `pad=207`).

const NN: usize = 255;

pub struct Rs256 {
    alpha_to: [u8; 256],
    index_of: [u8; 256],
    genpoly: Vec<i16>,
    nroots: usize,
    fcr: i32,
    prim: i32,
    pad: usize,
}

impl Rs256 {
    pub fn new(nroots: usize, pad: usize) -> Self {
        let gfpoly = 0x187u32;
        let mut alpha_to = [0u8; 256];
        let mut index_of = [0u8; 256];
        index_of[0] = 255;
        let mut sr = 1u32;
        for i in 0..NN {
            alpha_to[i] = sr as u8;
            index_of[sr as usize] = i as u8;
            sr <<= 1;
            if sr & 0x100 != 0 {
                sr ^= gfpoly;
            }
            sr &= 0xff;
        }
        debug_assert_eq!(sr, 1);

        let fcr = 120i32;
        let prim = 1i32;
        let mut genpoly = vec![0i16; nroots + 1];
        genpoly[0] = 1;
        let mut root = fcr * prim;
        for i in 0..nroots {
            genpoly[i + 1] = 1;
            for j in (1..=i).rev() {
                if genpoly[j] != 0 {
                    let idx = index_of[genpoly[j] as usize] as i32 + root;
                    genpoly[j] = genpoly[j - 1] ^ alpha_to[(idx % NN as i32) as usize] as i16;
                } else {
                    genpoly[j] = genpoly[j - 1];
                }
            }
            genpoly[0] = alpha_to
                [((index_of[genpoly[0] as usize] as i32 + root) % NN as i32) as usize]
                as i16;
            root += prim;
        }
        for g in genpoly.iter_mut() {
            *g = index_of[*g as usize] as i16;
        }

        Self {
            alpha_to,
            index_of,
            genpoly,
            nroots,
            fcr,
            prim,
            pad,
        }
    }

    fn modnn(&self, mut x: i32) -> i32 {
        while x >= NN as i32 {
            x -= NN as i32;
            x = (x >> 8) + (x & 0xff);
        }
        x
    }

    pub fn encode(&self, data: &[u8]) -> Vec<u8> {
        let nroots = self.nroots;
        let mut parity = vec![0u8; nroots];
        for &d in data {
            let feedback = self.index_of[(d ^ parity[0]) as usize] as i32;
            if feedback != 255 {
                for i in 1..nroots {
                    let g = self.genpoly[nroots - i];
                    if g != 255 {
                        parity[i] ^= self.alpha_to[self.modnn(g as i32 + feedback) as usize];
                    }
                }
            }
            parity.copy_within(1.., 0);
            if feedback != 255 {
                let g0 = self.genpoly[0];
                parity[nroots - 1] = self.alpha_to[self.modnn(g0 as i32 + feedback) as usize];
            } else {
                parity[nroots - 1] = 0;
            }
        }
        parity
    }

    /// Port of Phil Karn's `decode_rs.h` (dump978 / libfec).
    pub fn decode(&self, data: &mut [u8]) -> Result<usize, ()> {
        let nroots = self.nroots;
        let pad = self.pad;
        let nn = NN as i32;
        let a0 = nn;
        assert_eq!(data.len(), NN - pad);

        let mut s = vec![0i32; nroots];
        for i in 0..nroots {
            s[i] = i32::from(data[0]);
        }
        for j in 1..data.len() {
            for i in 0..nroots {
                if s[i] == 0 {
                    s[i] = i32::from(data[j]);
                } else {
                    let idx =
                        i32::from(self.index_of[s[i] as usize]) + (self.fcr + i as i32) * self.prim;
                    s[i] = i32::from(data[j]) ^ i32::from(self.alpha_to[self.modnn(idx) as usize]);
                }
            }
        }
        let mut syn_error = 0i32;
        for i in 0..nroots {
            syn_error |= s[i];
            s[i] = i32::from(self.index_of[s[i] as usize]);
        }
        if syn_error == 0 {
            return Ok(0);
        }

        let mut lambda = vec![0i32; nroots + 1];
        lambda[0] = 1;
        let mut b = vec![0i32; nroots + 1];
        for i in 0..=nroots {
            b[i] = i32::from(self.index_of[lambda[i] as usize]);
        }

        let mut r = 0usize;
        let mut el = 0usize;
        while {
            r += 1;
            r <= nroots
        } {
            let mut discr_r = 0u8;
            for i in 0..r {
                if lambda[i] != 0 && s[r - i - 1] != a0 {
                    discr_r ^= self.alpha_to[self
                        .modnn(i32::from(self.index_of[lambda[i] as usize]) + s[r - i - 1])
                        as usize];
                }
            }
            let discr_r = i32::from(self.index_of[discr_r as usize]);
            if discr_r == a0 {
                b.insert(0, a0);
                b.truncate(nroots + 1);
            } else {
                let mut t = vec![0i32; nroots + 1];
                t[0] = lambda[0];
                for i in 0..nroots {
                    if b[i] != a0 {
                        t[i + 1] = lambda[i + 1]
                            ^ i32::from(self.alpha_to[self.modnn(discr_r + b[i]) as usize]);
                    } else {
                        t[i + 1] = lambda[i + 1];
                    }
                }
                if 2 * el <= r - 1 {
                    el = r - el;
                    for i in 0..=nroots {
                        b[i] = if lambda[i] == 0 {
                            a0
                        } else {
                            self.modnn(i32::from(self.index_of[lambda[i] as usize]) - discr_r + nn)
                        };
                    }
                } else {
                    b.insert(0, a0);
                    b.truncate(nroots + 1);
                }
                lambda = t;
            }
        }

        let mut deg_lambda = 0usize;
        for i in 0..=nroots {
            lambda[i] = i32::from(self.index_of[lambda[i] as usize]);
            if lambda[i] != a0 {
                deg_lambda = i;
            }
        }

        let mut reg = vec![0i32; nroots + 1];
        reg[1..=nroots].copy_from_slice(&lambda[1..=nroots]);
        let mut count = 0usize;
        let mut root = vec![0i32; nroots];
        let mut loc = vec![0i32; nroots];
        let mut k = 0i32; // IPRIM-1
        for i in 1..=NN {
            let mut q = 1u8;
            for j in (1..=deg_lambda).rev() {
                if reg[j] != a0 {
                    reg[j] = self.modnn(reg[j] + j as i32);
                    q ^= self.alpha_to[reg[j] as usize];
                }
            }
            if q == 0 {
                root[count] = i as i32;
                loc[count] = k;
                count += 1;
                if count == deg_lambda {
                    break;
                }
            }
            k = self.modnn(k + 1);
        }
        if deg_lambda != count || count == 0 {
            return Err(());
        }

        let deg_omega = deg_lambda as i32 - 1;
        let mut omega = vec![a0; nroots + 1];
        for i in 0..=deg_omega {
            let mut tmp = 0u8;
            for j in 0..=i {
                if s[(i - j) as usize] != a0 && lambda[j as usize] != a0 {
                    tmp ^= self.alpha_to
                        [self.modnn(s[(i - j) as usize] + lambda[j as usize]) as usize];
                }
            }
            omega[i as usize] = i32::from(self.index_of[tmp as usize]);
        }

        for j in (0..count).rev() {
            let mut num1 = 0u8;
            for i in (0..=deg_omega).rev() {
                if omega[i as usize] != a0 {
                    num1 ^= self.alpha_to[self.modnn(omega[i as usize] + i * root[j]) as usize];
                }
            }
            let num2 = self.alpha_to[self.modnn(root[j] * (self.fcr - 1) + nn) as usize];
            let mut den = 0u8;
            let mut i = deg_lambda.min(nroots - 1) & !1;
            loop {
                if lambda[i + 1] != a0 {
                    den ^= self.alpha_to[self.modnn(lambda[i + 1] + i as i32 * root[j]) as usize];
                }
                if i < 2 {
                    break;
                }
                i -= 2;
            }
            if num1 != 0 && loc[j] >= pad as i32 && den != 0 {
                let mag = self.alpha_to[self.modnn(
                    i32::from(self.index_of[num1 as usize])
                        + i32::from(self.index_of[num2 as usize])
                        + nn
                        - i32::from(self.index_of[den as usize]),
                ) as usize];
                data[(loc[j] as usize) - pad] ^= mag;
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_uat_round_trip_and_correct() {
        let rs = Rs256::new(12, 225);
        let mut msg = [0u8; 18];
        for (i, b) in msg.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
        let parity = rs.encode(&msg);
        assert_eq!(parity.len(), 12);
        let mut cw = [0u8; 30];
        cw[..18].copy_from_slice(&msg);
        cw[18..].copy_from_slice(&parity);
        assert_eq!(rs.decode(&mut cw).unwrap(), 0, "clean codeword");
        cw[2] ^= 0x5A;
        let n = rs.decode(&mut cw).expect("single-error decode");
        assert_eq!(n, 1);
        assert_eq!(&cw[..18], &msg);
        cw[1] ^= 0x11;
        cw[7] ^= 0x22;
        cw[22] ^= 0x33;
        let n = rs.decode(&mut cw).expect("triple-error decode");
        assert_eq!(n, 3);
        assert_eq!(&cw[..18], &msg);
    }
}
