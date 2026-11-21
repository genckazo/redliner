import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import fs from 'node:fs'
import path from 'node:path'

// The bridge: the browser saves the board here, and Claude's command-line
// tool queues edits here for the browser to apply.
const DIR = path.resolve('board')
const ASSETS = path.join(DIR, 'assets')
fs.mkdirSync(ASSETS, { recursive: true })

const MIME = {
	'.png': 'image/png',
	'.jpg': 'image/jpeg',
	'.jpeg': 'image/jpeg',
	'.gif': 'image/gif',
	'.webp': 'image/webp',
	'.svg': 'image/svg+xml',
	'.mp4': 'video/mp4',
}

function readBody(req) {
	return new Promise((resolve, reject) => {
		const chunks = []
		req.on('data', (c) => chunks.push(c))
		req.on('end', () => resolve(Buffer.concat(chunks)))
		req.on('error', reject)
	})
}

function bridge() {
	let nextId = 1
	const pending = [] // { id, ops }
	const waiters = new Map() // id -> resolve

	const json = (res, code, data) => {
		res.statusCode = code
		res.setHeader('content-type', 'application/json')
		res.end(JSON.stringify(data))
	}

	return {
		name: 'board-bridge',
		configureServer(server) {
			server.middlewares.use(async (req, res, next) => {
				const url = new URL(req.url, 'http://x')
				const p = url.pathname
				try {
					if (p.startsWith('/assets/') && req.method === 'GET') {
						const file = path.join(ASSETS, path.basename(decodeURIComponent(p)))
						if (!fs.existsSync(file)) return json(res, 404, { error: 'not found' })
						res.setHeader('content-type', MIME[path.extname(file).toLowerCase()] || 'application/octet-stream')
						return fs.createReadStream(file).pipe(res)
					}
					if (!p.startsWith('/api/')) return next()

					if (p === '/api/board' && req.method === 'GET') {
						const file = path.join(DIR, 'board.json')
						if (!fs.existsSync(file)) return json(res, 200, null)
						res.setHeader('content-type', 'application/json')
						return fs.createReadStream(file).pipe(res)
					}
					if (p === '/api/board' && req.method === 'PUT') {
						const { snapshot, digest } = JSON.parse((await readBody(req)).toString())
						fs.writeFileSync(path.join(DIR, 'board.json'), JSON.stringify(snapshot))
						fs.writeFileSync(path.join(DIR, 'digest.json'), JSON.stringify(digest, null, 1))
						return json(res, 200, { ok: true })
					}
					if (p === '/api/ops' && req.method === 'GET') return json(res, 200, pending)
					if (p === '/api/ops' && req.method === 'POST') {
						const { ops } = JSON.parse((await readBody(req)).toString())
						const id = nextId++
						pending.push({ id, ops })
						const result = await new Promise((resolve) => {
							waiters.set(id, resolve)
							setTimeout(() => resolve(null), 15000)
						})
						waiters.delete(id)
						if (result === null) {
							const i = pending.findIndex((b) => b.id === id)
							if (i >= 0) pending.splice(i, 1)
							return json(res, 504, { error: 'The board is not open in a browser, so nothing was applied.' })
						}
						return json(res, 200, result)
					}
					if (p === '/api/ack' && req.method === 'POST') {
						const { id, result } = JSON.parse((await readBody(req)).toString())
						const i = pending.findIndex((b) => b.id === id)
						if (i >= 0) pending.splice(i, 1)
						waiters.get(id)?.(result)
						return json(res, 200, { ok: true })
					}
					if (p === '/api/upload' && req.method === 'POST') {
						const raw = path.basename(url.searchParams.get('name') || 'file')
						const name = Date.now() + '-' + raw.replace(/[^a-zA-Z0-9._-]/g, '_')
						fs.writeFileSync(path.join(ASSETS, name), await readBody(req))
						return json(res, 200, { src: '/assets/' + name })
					}
					if (p === '/api/shot' && req.method === 'POST') {
						fs.writeFileSync(path.join(DIR, 'shot.png'), await readBody(req))
						return json(res, 200, { ok: true })
					}
					if (p === '/api/ping' && req.method === 'POST') {
						fs.writeFileSync(path.join(DIR, 'ping'), new Date().toISOString())
						return json(res, 200, { ok: true })
					}
					return json(res, 404, { error: 'unknown endpoint' })
				} catch (e) {
					return json(res, 500, { error: String(e?.message || e) })
				}
			})
		},
	}
}

export default defineConfig({
	plugins: [react(), bridge()],
	server: { port: 5199, strictPort: true, watch: { ignored: ['**/board/**'] } },
})
