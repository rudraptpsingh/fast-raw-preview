import * as fs from 'node:fs/promises'
import type { RawPreviewResult } from './types'
import { extractJpegDimensions } from './dimensions'

interface BoxHeader {
  type: string
  offset: number
  size: number
  headerSize: number
}

function readBoxHeader(buf: Buffer, offset: number): BoxHeader | null {
  if (offset + 8 > buf.length) return null
  let size = buf.readUInt32BE(offset)
  const type = buf.subarray(offset + 4, offset + 8).toString('latin1')
  let headerSize = 8

  if (size === 1) {
    if (offset + 16 > buf.length) return null
    // 64-bit largesize
    const high = buf.readUInt32BE(offset + 8)
    const low = buf.readUInt32BE(offset + 12)
    size = high * 2 ** 32 + low
    headerSize = 16
  } else if (size === 0) {
    size = buf.length - offset
  }

  if (size < headerSize) return null
  return { type, offset, size, headerSize }
}

/**
 * Extract embedded JPEG preview from Canon CR3 files (ISO BMFF container).
 */
export async function extractCr3Preview(filePath: string): Promise<RawPreviewResult | null> {
  let fd: fs.FileHandle | null = null
  try {
    fd = await fs.open(filePath, 'r')

    // Read initial 32 KB to find 'ftyp' and top-level boxes
    const PEEK_SIZE = 65536
    const peekBuf = Buffer.allocUnsafe(PEEK_SIZE)
    const { bytesRead } = await fd.read(peekBuf, 0, PEEK_SIZE, 0)
    if (bytesRead < 16) return null

    // Check ftyp box
    if (peekBuf.subarray(4, 8).toString('latin1') !== 'ftyp') {
      return null
    }

    // Locate `moov` box
    let pos = 0
    let moovBox: BoxHeader | null = null
    while (pos < bytesRead) {
      const box = readBoxHeader(peekBuf, pos)
      if (!box) break
      if (box.type === 'moov') {
        moovBox = box
        break
      }
      pos += box.size
    }

    // If moov is beyond the first 64KB, read up to first 2MB or scan file
    if (!moovBox) {
      // Canon CR3 typically keeps moov in the first 256 KB - 1 MB
      const LARGE_PEEK = 2 * 1024 * 1024
      const largeBuf = Buffer.allocUnsafe(LARGE_PEEK)
      const { bytesRead: lRead } = await fd.read(largeBuf, 0, LARGE_PEEK, 0)
      pos = 0
      while (pos < lRead) {
        const box = readBoxHeader(largeBuf, pos)
        if (!box) break
        if (box.type === 'moov') {
          moovBox = box
          break
        }
        pos += box.size
      }
      if (!moovBox) return null
    }

    // Read entire moov box
    const moovPayloadSize = moovBox.size - moovBox.headerSize
    const moovBuf = Buffer.allocUnsafe(moovPayloadSize)
    const { bytesRead: mRead } = await fd.read(moovBuf, 0, moovPayloadSize, moovBox.offset + moovBox.headerSize)
    if (mRead < moovPayloadSize) return null

    // Inside moov, search for uuid boxes containing 'prvw' or 'THMB'
    let prvwPayload: Buffer | null = null
    let thmbPayload: Buffer | null = null

    let mPos = 0
    while (mPos < moovBuf.length) {
      const box = readBoxHeader(moovBuf, mPos)
      if (!box || mPos + box.size > moovBuf.length) break

      if (box.type === 'uuid' && box.size >= box.headerSize + 16) {
        // Canon preview UUID is 16 bytes
        const uuidInner = moovBuf.subarray(mPos + box.headerSize + 16, mPos + box.size)
        let uPos = 0
        while (uPos < uuidInner.length) {
          const sub = readBoxHeader(uuidInner, uPos)
          if (!sub || uPos + sub.size > uuidInner.length) break

          if (sub.type === 'prvw') {
            prvwPayload = Buffer.from(uuidInner.subarray(uPos + sub.headerSize, uPos + sub.size))
            break
          } else if (sub.type === 'THMB') {
            thmbPayload = Buffer.from(uuidInner.subarray(uPos + sub.headerSize, uPos + sub.size))
          }
          uPos += sub.size
        }
        if (prvwPayload) break
      }
      mPos += box.size
    }

    const payload = prvwPayload || thmbPayload
    if (!payload || payload.length < 20) return null

    // Search for JPEG SOI marker (0xFF, 0xD8) within first 32 bytes of the box payload
    let jpegStart = -1
    for (const offset of [16, 12, 8, 4, 0]) {
      if (offset + 2 <= payload.length && payload[offset] === 0xff && payload[offset + 1] === 0xd8) {
        jpegStart = offset
        break
      }
    }
    if (jpegStart === -1) {
      for (let i = 0; i < Math.min(64, payload.length - 2); i++) {
        if (payload[i] === 0xff && payload[i + 1] === 0xd8) {
          jpegStart = i
          break
        }
      }
    }
    if (jpegStart === -1) return null

    const jpegBuf = payload.subarray(jpegStart)
    const dims = extractJpegDimensions(jpegBuf)

    return {
      jpeg: jpegBuf,
      source: 'cr3-prvw',
      orientation: 1, // CR3 previews encode correct orientation or handle in standard EXIF
      width: dims?.width,
      height: dims?.height,
    }
  } catch {
    return null
  } finally {
    if (fd) await fd.close().catch(() => {})
  }
}

/**
 * Extract preview from an in-memory Canon CR3 buffer.
 */
export function extractCr3PreviewFromBuffer(buf: Buffer): RawPreviewResult | null {
  if (buf.length < 16) return null
  if (buf.subarray(4, 8).toString('latin1') !== 'ftyp') return null

  let pos = 0
  let moovBox: BoxHeader | null = null
  while (pos < buf.length) {
    const box = readBoxHeader(buf, pos)
    if (!box || pos + box.size > buf.length) break
    if (box.type === 'moov') {
      moovBox = box
      break
    }
    pos += box.size
  }
  if (!moovBox) return null

  const moovPayload = buf.subarray(moovBox.offset + moovBox.headerSize, moovBox.offset + moovBox.size)
  let prvwPayload: Buffer | null = null
  let thmbPayload: Buffer | null = null

  let mPos = 0
  while (mPos < moovPayload.length) {
    const box = readBoxHeader(moovPayload, mPos)
    if (!box || mPos + box.size > moovPayload.length) break

    if (box.type === 'uuid' && box.size >= box.headerSize + 16) {
      const uuidInner = moovPayload.subarray(mPos + box.headerSize + 16, mPos + box.size)
      let uPos = 0
      while (uPos < uuidInner.length) {
        const sub = readBoxHeader(uuidInner, uPos)
        if (!sub || uPos + sub.size > uuidInner.length) break
        if (sub.type === 'prvw') {
          prvwPayload = Buffer.from(uuidInner.subarray(uPos + sub.headerSize, uPos + sub.size))
          break
        } else if (sub.type === 'THMB') {
          thmbPayload = Buffer.from(uuidInner.subarray(uPos + sub.headerSize, uPos + sub.size))
        }
        uPos += sub.size
      }
      if (prvwPayload) break
    }
    mPos += box.size
  }

  const payload = prvwPayload || thmbPayload
  if (!payload || payload.length < 20) return null

  let jpegStart = -1
  for (const offset of [16, 12, 8, 4, 0]) {
    if (offset + 2 <= payload.length && payload[offset] === 0xff && payload[offset + 1] === 0xd8) {
      jpegStart = offset
      break
    }
  }
  if (jpegStart === -1) return null

  const jpegBuf = payload.subarray(jpegStart)
  const dims = extractJpegDimensions(jpegBuf)

  return {
    jpeg: jpegBuf,
    source: 'cr3-prvw',
    orientation: 1,
    width: dims?.width,
    height: dims?.height,
  }
}
