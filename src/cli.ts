#!/usr/bin/env node

import * as path from 'node:path'
import * as fs from 'node:fs/promises'
import { extractRawPreview, extractRawPreviews, isRawImage } from './index'

async function runCli() {
  const args = process.argv.slice(2)
  if (args.length === 0 || args.includes('-h') || args.includes('--help')) {
    console.log(`
Usage: fast-raw-preview <input-path> [options]

Ultra-fast embedded preview & metadata extractor for camera RAW files
(Canon CR2/CR3, Nikon NEF, Sony ARW, Fuji RAF, Olympus ORF, DNG, etc.)

Options:
  -o, --output <dir>    Directory to save extracted preview JPEGs
  -c, --concurrency <n> Number of parallel workers (default: 8)
  -j, --json            Output metadata summary in JSON format
  -v, --verbose         Print detailed execution timings
  -h, --help            Show this help message

Examples:
  npx fast-raw-preview _DSC1234.ARW -o ./previews
  npx fast-raw-preview /Volumes/SD_CARD/DCIM -o ./extracted -c 16
`)
    process.exit(0)
  }

  const inputPath = args[0]
  let outDir: string | null = null
  let concurrency = 8
  let jsonOutput = false
  let verbose = false

  for (let i = 1; i < args.length; i++) {
    if (args[i] === '-o' || args[i] === '--output') {
      outDir = args[++i]
    } else if (args[i] === '-c' || args[i] === '--concurrency') {
      concurrency = parseInt(args[++i], 10) || 8
    } else if (args[i] === '-j' || args[i] === '--json') {
      jsonOutput = true
    } else if (args[i] === '-v' || args[i] === '--verbose') {
      verbose = true
    }
  }

  const resolvedInput = path.resolve(inputPath)
  const st = await fs.stat(resolvedInput).catch(() => null)
  if (!st) {
    console.error(`Error: File or directory not found: ${resolvedInput}`)
    process.exit(1)
  }

  const filesToProcess: string[] = []
  if (st.isDirectory()) {
    const entries = await fs.readdir(resolvedInput, { withFileTypes: true })
    for (const entry of entries) {
      if (entry.isFile() && isRawImage(entry.name)) {
        filesToProcess.push(path.join(resolvedInput, entry.name))
      }
    }
  } else if (st.isFile()) {
    filesToProcess.push(resolvedInput)
  }

  if (filesToProcess.length === 0) {
    console.warn(`No supported RAW files found at ${resolvedInput}`)
    process.exit(0)
  }

  if (outDir) {
    await fs.mkdir(path.resolve(outDir), { recursive: true })
  }

  const startTime = Date.now()
  if (!jsonOutput) {
    console.log(`Processing ${filesToProcess.length} RAW file(s) with concurrency=${concurrency}...`)
  }

  const results = await extractRawPreviews(filesToProcess, { concurrency })
  const durationMs = Date.now() - startTime

  let successCount = 0
  const outputData: Array<Record<string, unknown>> = []

  for (let i = 0; i < filesToProcess.length; i++) {
    const file = filesToProcess[i]
    const res = results[i]
    if (res && res.jpeg) {
      successCount++
      let writtenTo: string | null = null
      if (outDir) {
        const base = path.basename(file, path.extname(file))
        const target = path.join(path.resolve(outDir), `${base}.jpg`)
        await fs.writeFile(target, res.jpeg)
        writtenTo = target
      }
      outputData.push({
        path: file,
        success: true,
        source: res.source,
        width: res.width,
        height: res.height,
        orientation: res.orientation,
        output: writtenTo,
      })
    } else {
      outputData.push({
        path: file,
        success: false,
        error: 'No embedded preview found',
      })
    }
  }

  if (jsonOutput) {
    console.log(JSON.stringify({ durationMs, count: filesToProcess.length, successCount, files: outputData }, null, 2))
  } else {
    console.log(`\nDone in ${durationMs}ms: ${successCount}/${filesToProcess.length} previews extracted.`)
    if (outDir) {
      console.log(`Previews saved to: ${path.resolve(outDir)}`)
    }
    if (verbose) {
      const avgMs = (durationMs / filesToProcess.length).toFixed(1)
      console.log(`Average: ${avgMs} ms/photo (${(filesToProcess.length / (durationMs / 1000)).toFixed(1)} fps)`)
    }
  }
}

runCli().catch((err) => {
  console.error('Fatal:', err)
  process.exit(1)
})
