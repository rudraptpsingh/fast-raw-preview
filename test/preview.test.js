const test = require('node:test')
const assert = require('node:assert')
const fs = require('node:fs/promises')
const path = require('node:path')
const os = require('node:os')

// We will test against the compiled dist/ or directly via compiled JS
const {
  extractRawPreview,
  isRawImage,
  extractJpegDimensions,
} = require('../dist/index.js')
const { extractRafPreviewFromBuffer } = require('../dist/raf.js')
const { extractCr3PreviewFromBuffer } = require('../dist/cr3.js')
const { extractTiffPreviewFromBuffer } = require('../dist/tiff.js')

function createSyntheticJpeg(width = 640, height = 480) {
  // Minimal valid JPEG header with SOI, SOF0, and EOI
  const buf = Buffer.alloc(32)
  buf[0] = 0xff
  buf[1] = 0xd8 // SOI

  // SOF0 marker
  buf[2] = 0xff
  buf[3] = 0xc0
  buf.writeUInt16BE(11, 4) // length
  buf[6] = 8 // precision
  buf.writeUInt16BE(height, 7)
  buf.writeUInt16BE(width, 9)
  buf[11] = 3 // components

  // EOI
  buf[12] = 0xff
  buf[13] = 0xd9
  return buf
}

test('JPEG dimension extractor parses SOF0 markers accurately', () => {
  const fakeJpeg = createSyntheticJpeg(1920, 1080)
  const dims = extractJpegDimensions(fakeJpeg)
  assert.notStrictEqual(dims, null)
  assert.strictEqual(dims.width, 1920)
  assert.strictEqual(dims.height, 1080)
})

test('Fujifilm RAF preview extraction locates JPEG at offset 0x54', () => {
  const jpeg = createSyntheticJpeg(800, 600)
  const rafBuf = Buffer.alloc(128 + jpeg.length)

  // Magic
  rafBuf.write('FUJIFILMCCD-RAW ', 0, 'ascii')
  // Offset 0x54 = 128
  rafBuf.writeUInt32BE(128, 0x54)
  // Length 0x58 = jpeg.length
  rafBuf.writeUInt32BE(jpeg.length, 0x58)
  // Copy JPEG bytes
  jpeg.copy(rafBuf, 128)

  const res = extractRafPreviewFromBuffer(rafBuf)
  assert.notStrictEqual(res, null)
  assert.strictEqual(res.source, 'raf-fast')
  assert.strictEqual(res.width, 800)
  assert.strictEqual(res.height, 600)
  assert.strictEqual(res.jpeg.length, jpeg.length)
})

test('TIFF parser walks IFD0 and extracts largest embedded JPEG preview', () => {
  const jpeg = createSyntheticJpeg(1280, 720)
  const tiffBuf = Buffer.alloc(256 + jpeg.length)

  // Header: II + 42 + IFD0 offset 8
  tiffBuf.write('II', 0, 'ascii')
  tiffBuf.writeUInt16LE(42, 2)
  tiffBuf.writeUInt32LE(8, 4)

  // IFD0: 3 entries
  // Entry 0: Orientation 0x0112 = 6 (Rotate 90 CW for portrait)
  // Entry 1: JPEGInterchangeFormat 0x0201 = 128
  // Entry 2: JPEGInterchangeFormatLength 0x0202 = jpeg.length
  tiffBuf.writeUInt16LE(3, 8)

  // Entry 0
  tiffBuf.writeUInt16LE(0x0112, 10)
  tiffBuf.writeUInt16LE(3, 12) // SHORT
  tiffBuf.writeUInt32LE(1, 14) // cnt
  tiffBuf.writeUInt16LE(6, 18) // Orientation 6

  // Entry 1
  tiffBuf.writeUInt16LE(0x0201, 22)
  tiffBuf.writeUInt16LE(4, 24) // LONG
  tiffBuf.writeUInt32LE(1, 26) // cnt
  tiffBuf.writeUInt32LE(128, 30) // Offset 128

  // Entry 2
  tiffBuf.writeUInt16LE(0x0202, 34)
  tiffBuf.writeUInt16LE(4, 36) // LONG
  tiffBuf.writeUInt32LE(1, 38) // cnt
  tiffBuf.writeUInt32LE(jpeg.length, 42) // Length

  // Next IFD = 0
  tiffBuf.writeUInt32LE(0, 46)

  // Copy JPEG at 128
  jpeg.copy(tiffBuf, 128)

  const res = extractTiffPreviewFromBuffer(tiffBuf)
  assert.notStrictEqual(res, null)
  assert.strictEqual(res.source, 'tiff-ifd')
  assert.strictEqual(res.orientation, 6)
  assert.strictEqual(res.width, 1280)
  assert.strictEqual(res.height, 720)
})

test('Canon CR3 parser walks moov -> uuid -> prvw box hierarchy', () => {
  const jpeg = createSyntheticJpeg(1600, 1200)

  // Construct prvw box: [size(4)][prvw][16-byte header][jpeg]
  const prvwSize = 8 + 16 + jpeg.length
  const prvwBox = Buffer.alloc(prvwSize)
  prvwBox.writeUInt32BE(prvwSize, 0)
  prvwBox.write('prvw', 4, 'latin1')
  jpeg.copy(prvwBox, 8 + 16)

  // Construct uuid box: [size(4)][uuid][16-byte uuid][prvwBox]
  const uuidSize = 8 + 16 + prvwSize
  const uuidBox = Buffer.alloc(uuidSize)
  uuidBox.writeUInt32BE(uuidSize, 0)
  uuidBox.write('uuid', 4, 'latin1')
  // Canon preview UUID
  Buffer.from('eaf42b5e1c984b88b9fbb7dc406e4d16', 'hex').copy(uuidBox, 8)
  prvwBox.copy(uuidBox, 24)

  // Construct moov box: [size(4)][moov][uuidBox]
  const moovSize = 8 + uuidSize
  const moovBox = Buffer.alloc(moovSize)
  moovBox.writeUInt32BE(moovSize, 0)
  moovBox.write('moov', 4, 'latin1')
  uuidBox.copy(moovBox, 8)

  // Construct CR3 container: [size(4)][ftyp][brand: crx ][version][moovBox]
  const ftypBox = Buffer.alloc(16)
  ftypBox.writeUInt32BE(16, 0)
  ftypBox.write('ftyp', 4, 'latin1')
  ftypBox.write('crx ', 8, 'latin1')

  const cr3Buf = Buffer.concat([ftypBox, moovBox])

  const res = extractCr3PreviewFromBuffer(cr3Buf)
  assert.notStrictEqual(res, null)
  assert.strictEqual(res.source, 'cr3-prvw')
  assert.strictEqual(res.width, 1600)
  assert.strictEqual(res.height, 1200)
})

test('isRawImage recognizes standard extensions', () => {
  assert.strictEqual(isRawImage('DSC_0001.ARW'), true)
  assert.strictEqual(isRawImage('IMG_1234.CR3'), true)
  assert.strictEqual(isRawImage('IMAGE.NEF'), true)
  assert.strictEqual(isRawImage('PHOTO.RAF'), true)
  assert.strictEqual(isRawImage('FRAME.DNG'), true)
  assert.strictEqual(isRawImage('test.jpg'), false)
  assert.strictEqual(isRawImage('notes.txt'), false)
})
