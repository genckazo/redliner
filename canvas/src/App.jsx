import { useEffect, useState } from 'react'
import { AssetRecordType, Tldraw, createShapeId, getSnapshot, toRichText, useEditor, useValue } from 'tldraw'
import 'tldraw/tldraw.css'

const sid = (key) => (String(key).startsWith('shape:') ? key : createShapeId(String(key)))
const short = (id) => String(id).replace(/^shape:/, '')
const round = (n) => Math.round(n)

let applyingAgentOps = false

// ---------- what Claude reads ----------

function buildDigest(editor) {
	const shapes = editor.getCurrentPageShapesSorted()
	const boxes = new Map()
	for (const s of shapes) {
		const b = editor.getShapePageBounds(s)
		if (b) boxes.set(s.id, b)
	}
	const items = shapes.map((s) => {
		const b = boxes.get(s.id)
		const item = {
			id: short(s.id),
			type: s.type === 'geo' ? s.props.geo : s.type,
			by: s.meta?.author || 'user',
		}
		if (item.by === 'user' && !s.meta?.answered) item.open = true
		const text = editor.getShapeUtil(s).getText(s)
		if (text) item.text = text
		if (b) Object.assign(item, { x: round(b.x), y: round(b.y), w: round(b.w), h: round(b.h) })
		if (s.props.color) item.color = s.props.color
		if (s.type === 'frame') item.text = s.props.name
		if (s.type === 'image' && s.props.assetId) item.src = editor.getAsset(s.props.assetId)?.props?.src?.slice(0, 200)
		if (s.type === 'arrow') {
			for (const bind of editor.getBindingsFromShape(s, 'arrow')) {
				item[bind.props.terminal === 'start' ? 'from' : 'to'] = short(bind.toId)
			}
		}
		// Pen strokes are how the user circles or crosses things out: say what they sit on.
		if ((s.type === 'draw' || s.type === 'highlight') && b) {
			const over = shapes
				.filter((o) => o.id !== s.id && o.type !== 'draw' && o.type !== 'highlight' && o.type !== 'arrow' && o.type !== 'frame')
				.filter((o) => boxes.get(o.id) && b.collides(boxes.get(o.id)))
				.map((o) => short(o.id))
			if (over.length) item.over = over
		}
		const parent = editor.getShape(s.parentId)
		if (parent) item.in = short(parent.id)
		return item
	})
	return { savedAt: new Date().toISOString(), openMarks: items.filter((i) => i.open).length, items }
}

async function save(editor) {
	await fetch('/api/board', {
		method: 'PUT',
		headers: { 'content-type': 'application/json' },
		body: JSON.stringify({ snapshot: getSnapshot(editor.store), digest: buildDigest(editor) }),
	})
}

async function shot(editor, ids) {
	const shapeIds = ids?.length ? ids.map(sid) : [...editor.getCurrentPageShapeIds()]
	if (!shapeIds.length) return { ok: false, reason: 'board is empty' }
	const b = editor.getCurrentPageBounds()
	const scale = ids?.length || !b ? 1 : Math.min(1, 2400 / Math.max(b.w, b.h))
	const { blob, width, height } = await editor.toImage(shapeIds, { format: 'png', background: true, scale, padding: 24 })
	await fetch('/api/shot', { method: 'POST', body: blob })
	return { ok: true, width, height, file: 'board/shot.png' }
}

// ---------- what Claude writes ----------

function imageSize(src) {
	return new Promise((resolve) => {
		const img = new Image()
		img.onload = () => resolve({ w: img.naturalWidth, h: img.naturalHeight })
		img.onerror = () => resolve({ w: 400, h: 300 })
		img.src = src
	})
}

async function createOne(editor, d) {
	const id = d.id ? sid(d.id) : createShapeId()
	const base = { id, x: d.x ?? 0, y: d.y ?? 0, meta: { author: 'claude' } }
	if (d.in) base.parentId = sid(d.in)
	const text = toRichText(d.text ?? '')

	if (d.kind === 'card') {
		editor.createShape({
			...base,
			type: 'geo',
			props: {
				geo: d.geo ?? 'rectangle',
				w: d.w ?? 320,
				h: d.h ?? 60,
				richText: text,
				color: d.color ?? 'violet',
				fill: d.fill ?? 'semi',
				dash: 'solid',
				size: d.size ?? 's',
				font: 'sans',
				align: d.align ?? 'start',
				verticalAlign: d.align === 'middle' ? 'middle' : 'start',
			},
		})
	} else if (d.kind === 'note') {
		editor.createShape({ ...base, type: 'note', props: { richText: text, color: d.color ?? 'violet', size: d.size ?? 's', font: 'sans' } })
	} else if (d.kind === 'text') {
		editor.createShape({
			...base,
			type: 'text',
			props: { richText: text, color: d.color ?? 'black', size: d.size ?? 's', font: 'sans', ...(d.w ? { w: d.w, autoSize: false } : {}) },
		})
	} else if (d.kind === 'frame') {
		editor.createShape({ ...base, type: 'frame', props: { w: d.w ?? 600, h: d.h ?? 400, name: d.text ?? '' } })
	} else if (d.kind === 'image') {
		const nat = await imageSize(d.src)
		const w = d.w ?? Math.min(nat.w, 480)
		const h = d.h ?? (w * nat.h) / nat.w
		const assetId = AssetRecordType.createId()
		const ext = (d.src.split('.').pop() || 'png').toLowerCase()
		editor.createAssets([
			{
				id: assetId,
				type: 'image',
				typeName: 'asset',
				props: { name: d.src.split('/').pop(), src: d.src, w: nat.w, h: nat.h, mimeType: ext === 'svg' ? 'image/svg+xml' : `image/${ext === 'jpg' ? 'jpeg' : ext}`, isAnimated: false },
				meta: {},
			},
		])
		editor.createShape({ ...base, type: 'image', props: { assetId, w, h } })
	} else if (d.kind === 'arrow') {
		const center = (key, fallback) => {
			const b = key && editor.getShapePageBounds(sid(key))
			return b ? { x: b.midX, y: b.midY } : fallback
		}
		const start = center(d.from, { x: d.x ?? 0, y: d.y ?? 0 })
		const end = center(d.to, { x: d.x2 ?? start.x + 100, y: d.y2 ?? start.y })
		editor.createShape({
			...base,
			x: start.x,
			y: start.y,
			type: 'arrow',
			props: { start: { x: 0, y: 0 }, end: { x: end.x - start.x, y: end.y - start.y }, richText: text, color: d.color ?? 'violet', size: 's', font: 'sans' },
		})
		const bindings = []
		for (const [terminal, key] of [['start', d.from], ['end', d.to]]) {
			if (key && editor.getShape(sid(key))) {
				bindings.push({ fromId: id, toId: sid(key), type: 'arrow', props: { terminal, normalizedAnchor: { x: 0.5, y: 0.5 }, isExact: false, isPrecise: false } })
			}
		}
		if (bindings.length) editor.createBindings(bindings)
	} else {
		return { error: `unknown kind "${d.kind}"` }
	}
	const b = editor.getShapePageBounds(id)
	return b ? { id: short(id), x: round(b.x), y: round(b.y), w: round(b.w), h: round(b.h) } : { id: short(id), error: 'not created' }
}

async function applyOp(editor, op) {
	if (op.type === 'create') {
		const out = []
		for (const d of op.shapes) out.push(await createOne(editor, d))
		return out
	}
	if (op.type === 'update') {
		const out = []
		for (const u of op.shapes) {
			const s = editor.getShape(sid(u.id))
			if (!s) {
				out.push({ id: u.id, error: 'no such shape' })
				continue
			}
			const props = {}
			if (u.text !== undefined) {
				if (s.type === 'frame') props.name = u.text
				else props.richText = toRichText(u.text)
			}
			for (const k of ['w', 'h', 'color', 'fill']) if (u[k] !== undefined) props[k] = u[k]
			editor.updateShape({ id: s.id, type: s.type, ...(u.x !== undefined ? { x: u.x } : {}), ...(u.y !== undefined ? { y: u.y } : {}), props })
			out.push({ id: u.id, ok: true })
		}
		return out
	}
	if (op.type === 'delete') {
		editor.deleteShapes(op.ids.map(sid))
		return { ok: true }
	}
	if (op.type === 'answer') {
		// Close the user's marks Claude has dealt with. No ids means all of them.
		const targets = op.ids?.length
			? op.ids.map((k) => editor.getShape(sid(k))).filter(Boolean)
			: editor.getCurrentPageShapes().filter((s) => s.meta?.author !== 'claude' && !s.meta?.answered)
		editor.updateShapes(targets.map((s) => ({ id: s.id, type: s.type, meta: { ...s.meta, answered: true } })))
		return { ok: true, closed: targets.length }
	}
	if (op.type === 'zoom') {
		if (op.ids?.length) {
			editor.select(...op.ids.map(sid))
			editor.zoomToSelection({ animation: { duration: 400 } })
			editor.selectNone()
		} else editor.zoomToFit({ animation: { duration: 400 } })
		return { ok: true }
	}
	if (op.type === 'shot') return await shot(editor, op.ids)
	return { error: `unknown op "${op.type}"` }
}

async function applyBatch(editor, batch) {
	const results = []
	applyingAgentOps = true
	editor.markHistoryStoppingPoint('claude')
	try {
		for (const op of batch.ops) {
			try {
				results.push(await applyOp(editor, op))
			} catch (e) {
				results.push({ error: String(e?.message || e) })
			}
		}
	} finally {
		applyingAgentOps = false
	}
	await save(editor)
	await fetch('/api/ack', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ id: batch.id, result: results }) })
}

// ---------- UI ----------

function Header() {
	const editor = useEditor()
	const counts = useValue(
		'counts',
		() => {
			const shapes = editor.getCurrentPageShapes()
			return { items: shapes.length, open: shapes.filter((s) => s.meta?.author !== 'claude' && !s.meta?.answered).length }
		},
		[editor]
	)
	const [state, setState] = useState('idle')
	const send = async () => {
		setState('sending')
		try {
			await save(editor)
			await shot(editor)
			await fetch('/api/ping', { method: 'POST' })
			setState('sent')
		} catch {
			setState('failed')
		}
		setTimeout(() => setState('idle'), 2500)
	}
	return (
		<div
			style={{
				pointerEvents: 'all',
				display: 'flex',
				alignItems: 'center',
				gap: 10,
				margin: 8,
				padding: '6px 6px 6px 12px',
				borderRadius: 10,
				background: 'var(--tl-color-panel, #fff)',
				boxShadow: '0 1px 4px rgba(0,0,0,.18)',
				font: '12px/1.2 system-ui, sans-serif',
				color: 'var(--tl-color-text-1, #222)',
			}}
		>
			<span>
				{counts.items} items · <b style={{ color: counts.open ? '#c2255c' : 'inherit' }}>{counts.open} open marks</b>
			</span>
			<button
				onClick={send}
				disabled={state === 'sending'}
				style={{ border: 0, borderRadius: 7, padding: '6px 10px', background: '#6741d9', color: '#fff', font: 'inherit', fontWeight: 600, cursor: 'pointer' }}
			>
				{state === 'sending' ? 'Sending…' : state === 'sent' ? 'Sent ✓' : state === 'failed' ? 'Failed' : 'Send to Claude'}
			</button>
		</div>
	)
}

const assetStore = {
	async upload(_asset, file) {
		const res = await fetch('/api/upload?name=' + encodeURIComponent(file.name || 'file'), { method: 'POST', body: file })
		return { src: (await res.json()).src }
	},
	resolve(asset) {
		return asset.props.src
	},
}

export default function App() {
	const [initial, setInitial] = useState(undefined)

	useEffect(() => {
		fetch('/api/board')
			.then((r) => r.json())
			.then((s) => setInitial(s))
			.catch(() => setInitial(null))
	}, [])

	if (initial === undefined) return null

	return (
		<div style={{ position: 'fixed', inset: 0 }}>
			<Tldraw
				snapshot={initial ?? undefined}
				assets={assetStore}
				components={{ SharePanel: Header }}
				onMount={(editor) => {
					window.editor = editor
					editor.getInitialMetaForShape = () => ({ author: applyingAgentOps ? 'claude' : 'user' })

					let timer
					const stopListening = editor.store.listen(
						() => {
							clearTimeout(timer)
							timer = setTimeout(() => save(editor).catch(() => {}), 800)
						},
						{ scope: 'document', source: 'all' }
					)

					let busy = false
					const poll = setInterval(async () => {
						if (busy) return
						busy = true
						try {
							const batches = await (await fetch('/api/ops')).json()
							for (const batch of batches) await applyBatch(editor, batch)
						} catch {
							// server restarting; try again next tick
						}
						busy = false
					}, 600)

					return () => {
						stopListening()
						clearInterval(poll)
						clearTimeout(timer)
					}
				}}
			/>
		</div>
	)
}
