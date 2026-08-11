import path from "node:path";
import { fileURLToPath } from "node:url";

// mt19937.js - Mersenne Twister MT19937 with a Python 3.14 random.Random-
// compatible API, used for the solver's decision RNG streams.
//
// The core is the standard mt19937ar algorithm (Nishimura/Matsumoto) as used
// by CPython's _randommodule.c.  Seeding replicates CPython exactly:
//
//   * int seed -> use |seed|; key = little-endian 32-bit words with
//     keyused = ceil(bit_length(|seed|) / 32) (min 1); init_by_array(key).
//     (Not init_genrand: CPython routes every int through init_by_array.)
//   * null seed -> urandom-backed init_by_array(624 words) (non-deterministic).
//
// getrandbits(k) assembles little-endian 32-bit words like CPython, so
// `random()`, `choice()` and `shuffle()` produce bit-identical streams.

const N = 624;
const M = 397;
const MATRIX_A = 0x9908b0df;
const UPPER_MASK = 0x80000000;
const LOWER_MASK = 0x7fffffff;

const FLOAT_DIV = 1.0 / 9007199254740992.0; // 1 / 2**53

// Standard single-word init.  (Kept for the core and init_by_array's head.)
function initGenrand(state, s) {
  state[0] = s >>> 0;
  for (let i = 1; i < N; i++) {
    state[i] =
      (Math.imul(1812433253, state[i - 1] ^ (state[i - 1] >>> 30)) + i) >>> 0;
  }
}

// Standard init_by_array (CPython init_by_array verbatim).
function initByArray(state, key) {
  initGenrand(state, 19650218);
  let i = 1;
  let j = 0;
  let k = N > key.length ? N : key.length;
  for (; k; k--) {
    state[i] =
      ((state[i] ^
        Math.imul(state[i - 1] ^ (state[i - 1] >>> 30), 1664525)) +
        key[j] +
        j) >>>
      0;
    i++;
    j++;
    if (i >= N) {
      state[0] = state[N - 1];
      i = 1;
    }
    if (j >= key.length) j = 0;
  }
  for (k = N - 1; k; k--) {
    state[i] =
      ((state[i] ^
        Math.imul(state[i - 1] ^ (state[i - 1] >>> 30), 1566083941)) -
        i) >>>
      0;
    i++;
    if (i >= N) {
      state[0] = state[N - 1];
      i = 1;
    }
  }
  state[0] = 0x80000000;
}

// The MT19937 core.  Also usable directly (genrand_uint32 honours the
// twist-on-first-call contract, matching CPython genrand_uint32).
export class Mt19937 {
  constructor(seed) {
    this.state = new Uint32Array(N);
    this.index = N;
    this.seed(seed);
  }

  seed(seed) {
    if (seed === null || seed === undefined) {
      const key = new Uint32Array(N);
      for (let i = 0; i < N; i++) key[i] = Math.floor(Math.random() * 0x100000000);
      initByArray(this.state, key);
      this.index = N;
      return;
    }
    const n = BigInt(seed);
    const abs = n < 0n ? -n : n;
    const bits = abs === 0n ? 0 : abs.toString(2).length;
    const keyused = bits === 0 ? 1 : Math.ceil(bits / 32);
    const key = new Array(keyused);
    for (let i = 0; i < keyused; i++) {
      key[i] = Number((abs >> BigInt(32 * i)) & 0xffffffffn);
    }
    initByArray(this.state, key);
    this.index = N;
  }

  genrandUint32() {
    if (this.index >= N) {
      for (let i = 0; i < N; i++) {
        const y = (this.state[i] & UPPER_MASK) | (this.state[(i + 1) % N] & LOWER_MASK);
        this.state[i] =
          this.state[(i + M) % N] ^ (y >>> 1) ^ (y & 1 ? MATRIX_A : 0);
      }
      this.index = 0;
    }
    let y = this.state[this.index++];
    y ^= y >>> 11;
    y ^= (y << 7) & 0x9d2c5680;
    y ^= (y << 15) & 0xefc60000;
    y ^= y >>> 18;
    return y >>> 0;
  }

  // Serializable state snapshot/restore (used to carry a decision RNG across
  // worker threads so the main-thread stream advances exactly like CPython's
  // single shared RNG).
  snapshot() {
    return { state: Array.from(this.state), index: this.index };
  }

  static fromState(s) {
    const m = Object.create(this.prototype);
    m.state = Uint32Array.from(s.state);
    m.index = s.index;
    return m;
  }

  restore(s) {
    this.state = Uint32Array.from(s.state);
    this.index = s.index;
  }

  // CPython random(): 53-bit double in [0, 1).
  random() {
    const a = this.genrandUint32() >>> 5;
    const b = this.genrandUint32() >>> 6;
    return (a * 67108864.0 + b) * FLOAT_DIV;
  }

  // CPython getrandbits(k): returns a BigInt (Python returns an int).
  getrandbits(k) {
    if (k === 0) return 0n;
    if (k <= 32) return BigInt(this.genrandUint32() >>> (32 - k));
    const words = Math.ceil(k / 32);
    let result = 0n;
    let bits = k;
    for (let i = 0; i < words; i++) {
      let r = this.genrandUint32();
      if (bits < 32) r >>>= 32 - bits;
      result |= BigInt(r) << BigInt(32 * i);
      bits -= 32;
    }
    return result;
  }

  // CPython _randbelow_with_getrandbits: returns a BigInt in [0, n).
  _randbelow(n) {
    const nb = typeof n === "bigint" ? n : BigInt(n);
    if (nb <= 0n) throw new RangeError("_randbelow requires n > 0");
    const k = nb.toString(2).length;
    let r;
    do {
      r = this.getrandbits(k);
    } while (r >= nb);
    return r;
  }

  choice(seq) {
    if (seq.length === 0) throw new RangeError("Cannot choose from an empty sequence");
    return seq[Number(this._randbelow(seq.length))];
  }

  randrange(start, stop, step) {
    if (stop === undefined) {
      if (step !== undefined) throw new TypeError("Missing a non-None stop argument");
      if (start > 0) return Number(this._randbelow(start));
      throw new RangeError("empty range for randrange()");
    }
    const istop = stop;
    const width = istop - start;
    const istep = step === undefined ? 1 : step;
    if (typeof width === "bigint") {
      // BigInt path: mirrors CPython randrange exactly (returns BigInt).
      if (istep === 1) {
        if (width > 0n) return start + this._randbelow(width);
        throw new RangeError(`empty range in randrange(${start}, ${stop})`);
      }
      let n;
      if (istep > 0n) n = (width + istep - 1n) / istep;
      else if (istep < 0n) n = (width + istep + 1n) / istep;
      else throw new RangeError("zero step for randrange()");
      if (n <= 0n)
        throw new RangeError(`empty range in randrange(${start}, ${stop}, ${istep})`);
      return start + istep * this._randbelow(n);
    }
    if (istep === 1) {
      if (width > 0) return start + Number(this._randbelow(width));
      throw new RangeError(`empty range in randrange(${start}, ${stop})`);
    }
    let n;
    if (istep > 0) n = Math.floor((width + istep - 1) / istep);
    else if (istep < 0) n = Math.floor((width + istep + 1) / istep);
    else throw new RangeError("zero step for randrange()");
    if (n <= 0) throw new RangeError(`empty range in randrange(${start}, ${stop}, ${istep})`);
    return start + istep * Number(this._randbelow(n));
  }

  randint(a, b) {
    return this.randrange(a, b + 1);
  }

  shuffle(seq) {
    for (let i = seq.length - 1; i > 0; i--) {
      const j = Number(this._randbelow(i + 1));
      const tmp = seq[i];
      seq[i] = seq[j];
      seq[j] = tmp;
    }
  }
}

// Python's random.Random(seed) drop-in for the solver / bot decision RNGs.
export class Random extends Mt19937 {}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  const c1 = new Mt19937(5489);
  const ok1 = c1.getrandbits(32) === 3382763572n;
  const c2 = new Random(4242);
  const ok2 = Math.abs(c2.random() - 0.8624508153567833) < 1e-15;
  const c3 = new Random(2n ** 40n + 7n);
  const ok3 = c3.getrandbits(32) === 2635837658n;
  console.log(`mt19937 selfcheck: ${ok1 && ok2 && ok3 ? "PASS" : "FAIL"}`);
  process.exit(ok1 && ok2 && ok3 ? 0 : 1);
}
