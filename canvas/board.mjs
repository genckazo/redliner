#!/usr/bin/env node
// Claude's side of the shared canvas.
//
//   node board.mjs read [--open]      what is on the board (--open: only unanswered user marks)
//   node board.mjs ops '<json>'       apply edits; json is an array of ops, or a path to a .json file
//   node board.mjs img <file>         copy an image into the board's assets, print its src
//   node board.mjs shot [id ...]      export a PNG of the board (or of some shapes) to board/shot.png
//   node board.mjs wait               block until the user presses "Send to Claude"
//
// Ops:
//   {type:"create", shapes:[{kind:"card"|"note"|"text"|"frame"|"image"|"arrow", id?, x, y, w?, h?, text?, color?, src?, from?, to?, in?}]}
//   {type:"update", shapes:[{id, x?, y?, w?, h?, text?, color?}]}
//   {type:"delete", ids:[...]}
//   {type:"answer", ids?:[...]}       close user marks (all open ones when ids is omitted)
//   {type:"zoom", ids?:[...]}
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = path.dirname(fileURLToPath(import.meta.url))
const DIR = path.join(ROOT, 'board')
const URL_BASE = process.env.BOARD_URL || 'http://localhost:5199'
const [cmd, ...args] = process.argv.slice(2)

async function send(ops) {
	const res = await fetch(URL_BASE + '/api/ops', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ ops }) })
	const body = await res.json()
	if (!res.ok) {
		console.error(body.error || res.statusText)
		process.exit(1)
	}
	return body
}

if (cmd === 'read') {
	const file = path.join(DIR, 'digest.json')
	if (!fs.existsSync(file)) {
		console.log('The board has not been saved yet.')
		process.exit(0)
	}
	const d = JSON.parse(fs.readFileSync(file, 'utf8'))
	const items = args.includes('--open') ? d.items.filter((i) => i.open) : d.items
	console.log(`saved ${d.savedAt} · ${d.items.length} items · ${d.openMarks} open marks`)
	for (const i of items) {
		const bits = [`${i.open ? '●' : ' '} ${i.by === 'claude' ? 'C' : 'U'} ${i.id} [${i.type}] @${i.x},${i.y} ${i.w}x${i.h}`]
		if (i.color) bits.push(i.color)
		if (i.in) bits.push(`in:${i.in}`)
		if (i.from || i.to) bits.push(`${i.from ?? '?'} -> ${i.to ?? '?'}`)
		if (i.over) bits.push(`over:${i.over.join(',')}`)
		if (i.src) bits.push(`src:${i.src}`)
		if (i.text) bits.push(JSON.stringify(i.text))
		console.log(bits.join(' '))
	}
} else if (cmd === 'ops') {
	const raw = fs.existsSync(args[0]) ? fs.readFileSync(args[0], 'utf8') : args[0]
	const ops = JSON.parse(raw)
	console.log(JSON.stringify(await send(Array.isArray(ops) ? ops : [ops])))
} else if (cmd === 'img') {
	const name = Date.now() + '-' + path.basename(args[0]).replace(/[^a-zA-Z0-9._-]/g, '_')
	fs.mkdirSync(path.join(DIR, 'assets'), { recursive: true })
	fs.copyFileSync(args[0], path.join(DIR, 'assets', name))
	console.log('/assets/' + name)
} else if (cmd === 'shot') {
	console.log(JSON.stringify(await send([{ type: 'shot', ids: args }])))
} else if (cmd === 'wait') {
	const ping = path.join(DIR, 'ping')
	while (!fs.existsSync(ping)) await new Promise((r) => setTimeout(r, 1000))
	fs.rmSync(ping)
	console.log('The user pressed "Send to Claude". Read the board: node board.mjs read --open')
} else {
	console.log(fs.readFileSync(fileURLToPath(import.meta.url), 'utf8').split('\n').slice(1, 17).join('\n'))
}
