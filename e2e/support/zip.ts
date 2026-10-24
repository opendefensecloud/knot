import { inflateRawSync } from "node:zlib";

const EOCD_SIG = 0x06054b50;
const CENTRAL_SIG = 0x02014b50;
const LOCAL_SIG = 0x04034b50;
const EOCD_MIN = 22;
const STORED = 0;
const DEFLATE = 8;

/**
 * Read a zip archive into name → bytes, without a zip dependency.
 *
 * Walks the central directory (the authoritative entry list), then reads each
 * entry's data after its local header. Handles the two methods the exporters
 * write: stored (returned as is) and deflate (inflated with zlib). Zip64 and
 * other methods throw, since the exporters never produce them.
 */
export function zipEntries(buf: Buffer): Map<string, Buffer> {
  const eocd = findEocd(buf);
  const count = buf.readUInt16LE(eocd + 10);
  let p = buf.readUInt32LE(eocd + 16);
  const out = new Map<string, Buffer>();
  for (let i = 0; i < count; i++) {
    if (buf.readUInt32LE(p) !== CENTRAL_SIG) throw new Error(`zip: bad central header at ${p}`);
    const method = buf.readUInt16LE(p + 10);
    const compressed = buf.readUInt32LE(p + 20);
    const nameLen = buf.readUInt16LE(p + 28);
    const extraLen = buf.readUInt16LE(p + 30);
    const commentLen = buf.readUInt16LE(p + 32);
    const local = buf.readUInt32LE(p + 42);
    const name = buf.toString("utf8", p + 46, p + 46 + nameLen);
    if (compressed === 0xffffffff || local === 0xffffffff) throw new Error(`zip: zip64 entry ${name}`);

    if (buf.readUInt32LE(local) !== LOCAL_SIG) throw new Error(`zip: bad local header for ${name}`);
    const start = local + 30 + buf.readUInt16LE(local + 26) + buf.readUInt16LE(local + 28);
    const data = buf.subarray(start, start + compressed);
    if (method === STORED) out.set(name, Buffer.from(data));
    else if (method === DEFLATE) out.set(name, inflateRawSync(data));
    else throw new Error(`zip: unsupported method ${method} for ${name}`);

    p += 46 + nameLen + extraLen + commentLen;
  }
  return out;
}

/** The end-of-central-directory record sits at the end, before a comment of up to 64 KiB. */
function findEocd(buf: Buffer): number {
  const floor = Math.max(0, buf.length - EOCD_MIN - 0xffff);
  for (let i = buf.length - EOCD_MIN; i >= floor; i--) {
    if (buf.readUInt32LE(i) === EOCD_SIG) return i;
  }
  throw new Error("zip: end of central directory not found");
}
