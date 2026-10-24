import * as fs from 'node:fs/promises'
import type { RawPreviewResult } from './types'
import { extractJpegDimensions } from './dimensions'

const RAF_MAGIC = 'FUJIFILMCCD-RAW '

/**
 * Extract embedded JPEG preview from a Fujifilm RAF file.
 * Fast seek: reads the 120-byte RAF header, gets offset and length, then reads only those JPEG bytes.
 */
export async function extractRafPreview(filePath: string): Promise<RawPreviewResult | null> {
  let fd: fs.FileHandle | null = null
  try {
    fd = await fs.open(filePath, 'r')
    const headerBuf = Buffer.allocUnsafe(120)
    const { bytesRead } = await fd.read(headerBuf, 0, 120, 0)
    if (bytesRead < 0x60) return null

    if (headerBuf.subarray(0, 16).toString('ascii') !== RAF_MAGIC) {
      return null
    }

    const jpegOffset = headerBuf.readUInt32BE(0x54)
    const jpegLength = headerBuf.readUInt32BE(0x58)

    if (jpegOffset === 0 || jpegLength < 4) return null

    const jpegBuf = Buffer.allocUnsafe(jpegLength)
    const { bytesRead: readJpeg } = await fd.read(jpegBuf, 0, jpegLength, jpegOffset)
    if (readJpeg < jpegLength) return null

    // Verify JPEG SOI marker
    if (jpegBuf[0] !== 0xff || jpegBuf[1] !== 0xd8) {
      return null
    }

    const dims = extractJpegDimensions(jpegBuf)

    return {
      jpeg: jpegBuf,
      source: 'raf-fast',
      orientation: 1, // RAF previews carry standard EXIF orientation inside APP1
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
 * Extract embedded JPEG preview from an in-memory Fujifilm RAF buffer.
 */
export function extractRafPreviewFromBuffer(buf: Buffer): RawPreviewResult | null {
  if (buf.length < 0x60) return null
  if (buf.subarray(0, 16).toString('ascii') !== RAF_MAGIC) return null

  const jpegOffset = buf.readUInt32BE(0x54)
  const jpegLength = buf.readUInt32BE(0x58)

  if (jpegOffset + jpegLength > buf.length || jpegLength < 4) return null

  const jpegBuf = buf.subarray(jpegOffset, jpegOffset + jpegLength)
  if (jpegBuf[0] !== 0xff || jpegBuf[1] !== 0xd8) return null

  const dims = extractJpegDimensions(jpegBuf)

  return {
    jpeg: Buffer.from(jpegBuf),
    source: 'raf-fast',
    orientation: 1,
    width: dims?.width,
    height: dims?.height,
  }
}
