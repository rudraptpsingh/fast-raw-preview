import * as fs from 'node:fs/promises'
import type { RawPreviewResult } from './types'
import { extractJpegDimensions } from './dimensions'

export const TIFF_RAW_EXTENSIONS = new Set([
  '.arw',
  '.srf',
  '.sr2', // Sony
  '.cr2', // Canon CR2 (CR3 is ISOBMFF)
  '.nef',
  '.nrw', // Nikon
  '.dng', // Adobe DNG
  '.orf', // Olympus
  '.rw2', // Panasonic
  '.pef', // Pentax
  '.3fr', // Hasselblad
  '.dcr',
  '.kdc', // Kodak
  '.erf', // Epson
  '.mrw', // Minolta
])

/**
 * Extract embedded JPEG preview and IFD0 Orientation from TIFF-based RAW files.
 * Uses targeted reads to inspect only IFDs and SubIFDs without loading the multi-MB image.
 */
export async function extractTiffPreview(filePath: string): Promise<RawPreviewResult | null> {
  let fd: fs.FileHandle | null = null
  try {
    fd = await fs.open(filePath, 'r')

    // Read first 64 KB for header and primary IFD0
    const HEADER_SIZE = 65536
    const hdr = Buffer.allocUnsafe(HEADER_SIZE)
    const { bytesRead } = await fd.read(hdr, 0, HEADER_SIZE, 0)
    if (bytesRead < 8) return null

    const byteOrder = hdr.subarray(0, 2).toString('ascii')
    if (byteOrder !== 'II' && byteOrder !== 'MM') return null
    const le = byteOrder === 'II'

    const r16 = (b: Buffer, o: number): number => (le ? b.readUInt16LE(o) : b.readUInt16BE(o))
    const r32 = (b: Buffer, o: number): number => (le ? b.readUInt32LE(o) : b.readUInt32BE(o))

    const magic = r16(hdr, 2)
    // 42 = standard TIFF, 0x0055 (85) = Panasonic RW2
    if (magic !== 42 && magic !== 85) return null

    const ifd0 = r32(hdr, 4)
    if (ifd0 < 8 || ifd0 + 2 > bytesRead) return null

    const ifdCache = new Map<number, Buffer>()
    async function readIfdAt(offset: number): Promise<{ buf: Buffer; base: number; len: number } | null> {
      if (offset + 2 <= bytesRead) {
        return { buf: hdr, base: 0, len: bytesRead }
      }
      if (ifdCache.has(offset)) {
        const cached = ifdCache.get(offset)!
        return { buf: cached, base: offset, len: cached.length }
      }
      const peekBuf = Buffer.allocUnsafe(2)
      const { bytesRead: p } = await fd!.read(peekBuf, 0, 2, offset)
      if (p < 2) return null
      const count = le ? peekBuf.readUInt16LE(0) : peekBuf.readUInt16BE(0)
      if (count === 0 || count > 512) return null
      const ifdSize = 2 + count * 12 + 4
      const ifdBuf = Buffer.allocUnsafe(ifdSize)
      const { bytesRead: r } = await fd!.read(ifdBuf, 0, ifdSize, offset)
      if (r < ifdSize) return null
      ifdCache.set(offset, ifdBuf)
      return { buf: ifdBuf, base: offset, len: ifdSize }
    }

    const toVisit: number[] = [ifd0]
    const visited = new Set<number>()
    let best: { offset: number; length: number } | null = null
    let ifd0Orientation = 1

    while (toVisit.length > 0) {
      const ifdOff = toVisit.shift()!
      if (visited.has(ifdOff) || visited.size >= 24) continue
      if (ifdOff < 8) continue
      visited.add(ifdOff)

      const ifdData = await readIfdAt(ifdOff)
      if (!ifdData) continue
      const { buf, base, len } = ifdData
      const localOff = ifdOff - base

      if (localOff + 2 > len) continue
      const count = r16(buf, localOff)
      if (count === 0 || count > 512) continue

      let jpegOff = 0
      let jpegLen = 0

      for (let i = 0; i < count; i++) {
        const e = localOff + 2 + i * 12
        if (e + 12 > len) break
        const tag = r16(buf, e)
        const type = r16(buf, e + 2)
        const cnt = r32(buf, e + 4)
        const vOff = e + 8

        if (tag === 0x0112 && type === 3 && ifdOff === ifd0) {
          // Orientation tag in IFD0
          const o = r16(buf, vOff)
          if (o >= 1 && o <= 8) ifd0Orientation = o
        } else if (tag === 0x0201) {
          // JPEGInterchangeFormat
          jpegOff = r32(buf, vOff)
        } else if (tag === 0x0202) {
          // JPEGInterchangeFormatLength
          jpegLen = r32(buf, vOff)
        } else if ((tag === 0x014a || tag === 0xc51b) && type === 4) {
          // SubIFDs or Sony PreviewIFD
          if (cnt === 1) {
            const sub = r32(buf, vOff)
            if (sub > 0) toVisit.push(sub)
          } else if (cnt > 1 && cnt <= 16) {
            const arrOff = r32(buf, vOff)
            const arrData = await readIfdAt(arrOff)
            if (arrData) {
              const aLocal = arrOff - arrData.base
              for (let j = 0; j < cnt; j++) {
                if (aLocal + (j + 1) * 4 <= arrData.len) {
                  const sub = r32(arrData.buf, aLocal + j * 4)
                  if (sub > 0) toVisit.push(sub)
                }
              }
            }
          }
        }
      }

      if (jpegOff > 0 && jpegLen > 0) {
        if (!best || jpegLen > best.length) {
          best = { offset: jpegOff, length: jpegLen }
        }
      }

      // Next IFD in pointer chain
      const nextPtrOff = localOff + 2 + count * 12
      if (nextPtrOff + 4 <= len) {
        const nextIfd = r32(buf, nextPtrOff)
        if (nextIfd > 0) toVisit.push(nextIfd)
      }
    }

    if (!best || best.length < 4) return null

    // Read only the JPEG preview bytes
    const previewBuf = Buffer.allocUnsafe(best.length)
    const { bytesRead: pRead } = await fd.read(previewBuf, 0, best.length, best.offset)
    if (pRead < best.length) return null

    // Verify JPEG SOI
    if (previewBuf[0] !== 0xff || previewBuf[1] !== 0xd8) {
      return null
    }

    const dims = extractJpegDimensions(previewBuf)

    return {
      jpeg: previewBuf,
      source: 'tiff-ifd',
      orientation: ifd0Orientation,
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
 * Extract preview from an in-memory TIFF buffer.
 */
export function extractTiffPreviewFromBuffer(buf: Buffer): RawPreviewResult | null {
  if (buf.length < 8) return null
  const byteOrder = buf.subarray(0, 2).toString('ascii')
  if (byteOrder !== 'II' && byteOrder !== 'MM') return null
  const le = byteOrder === 'II'

  const r16 = (b: Buffer, o: number): number => (le ? b.readUInt16LE(o) : b.readUInt16BE(o))
  const r32 = (b: Buffer, o: number): number => (le ? b.readUInt32LE(o) : b.readUInt32BE(o))

  const magic = r16(buf, 2)
  if (magic !== 42 && magic !== 85) return null

  const ifd0 = r32(buf, 4)
  if (ifd0 < 8 || ifd0 + 2 > buf.length) return null

  const toVisit: number[] = [ifd0]
  const visited = new Set<number>()
  let best: { offset: number; length: number } | null = null
  let ifd0Orientation = 1

  while (toVisit.length > 0) {
    const ifdOff = toVisit.shift()!
    if (visited.has(ifdOff) || visited.size >= 24) continue
    if (ifdOff + 2 > buf.length) continue
    visited.add(ifdOff)

    const count = r16(buf, ifdOff)
    if (count === 0 || count > 512) continue

    let jpegOff = 0
    let jpegLen = 0

    for (let i = 0; i < count; i++) {
      const e = ifdOff + 2 + i * 12
      if (e + 12 > buf.length) break
      const tag = r16(buf, e)
      const type = r16(buf, e + 2)
      const cnt = r32(buf, e + 4)
      const vOff = e + 8

      if (tag === 0x0112 && type === 3 && ifdOff === ifd0) {
        const o = r16(buf, vOff)
        if (o >= 1 && o <= 8) ifd0Orientation = o
      } else if (tag === 0x0201) {
        jpegOff = r32(buf, vOff)
      } else if (tag === 0x0202) {
        jpegLen = r32(buf, vOff)
      } else if ((tag === 0x014a || tag === 0xc51b) && type === 4) {
        if (cnt === 1) {
          const sub = r32(buf, vOff)
          if (sub > 0) toVisit.push(sub)
        } else if (cnt > 1 && cnt <= 16) {
          const arrOff = r32(buf, vOff)
          for (let j = 0; j < cnt; j++) {
            if (arrOff + (j + 1) * 4 <= buf.length) {
              const sub = r32(buf, arrOff + j * 4)
              if (sub > 0) toVisit.push(sub)
            }
          }
        }
      }
    }

    if (jpegOff > 0 && jpegLen > 0 && jpegOff + jpegLen <= buf.length) {
      if (!best || jpegLen > best.length) {
        best = { offset: jpegOff, length: jpegLen }
      }
    }

    const nextPtrOff = ifdOff + 2 + count * 12
    if (nextPtrOff + 4 <= buf.length) {
      const nextIfd = r32(buf, nextPtrOff)
      if (nextIfd > 0) toVisit.push(nextIfd)
    }
  }

  if (!best || best.length < 4 || best.offset + best.length > buf.length) return null

  const previewBuf = buf.subarray(best.offset, best.offset + best.length)
  if (previewBuf[0] !== 0xff || previewBuf[1] !== 0xd8) return null

  const dims = extractJpegDimensions(previewBuf)

  return {
    jpeg: Buffer.from(previewBuf),
    source: 'tiff-ifd',
    orientation: ifd0Orientation,
    width: dims?.width,
    height: dims?.height,
  }
}
