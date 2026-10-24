/**
 * Extract width and height from a JPEG buffer by scanning frame markers (SOF0/SOF2).
 * Very fast: inspects only marker headers, never decodes image pixels.
 */
export function extractJpegDimensions(buf: Buffer): { width: number; height: number } | null {
  if (buf.length < 4 || buf[0] !== 0xff || buf[1] !== 0xd8) {
    return null
  }

  let pos = 2
  while (pos + 4 <= buf.length) {
    if (buf[pos] !== 0xff) {
      return null
    }

    const marker = buf[pos + 1]

    // Standalone markers without payload
    if (marker === 0xd8 || marker === 0xd9 || (marker >= 0xd0 && marker <= 0xd7)) {
      pos += 2
      continue
    }

    if (pos + 4 > buf.length) return null

    const len = buf.readUInt16BE(pos + 2)
    if (len < 2) return null

    // SOF0 (Baseline DCT), SOF1 (Extended Sequential), SOF2 (Progressive DCT)
    if (marker === 0xc0 || marker === 0xc1 || marker === 0xc2) {
      if (pos + 9 <= buf.length) {
        const height = buf.readUInt16BE(pos + 5)
        const width = buf.readUInt16BE(pos + 7)
        return { width, height }
      }
      return null
    }

    // Start of Scan (SOS): image data starts here, no more headers
    if (marker === 0xda) {
      break
    }

    pos += 2 + len
  }

  return null
}
