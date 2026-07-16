#!/usr/bin/env node
// End-to-end test for discord-channel.mjs: spawns the bridge with a mock
// Discord gateway (minimal RFC 6455 websocket server) and a mock REST
// API, then drives it over real stdio.
//
//   node plugins/discord/server/discord-channel.test.mjs
//
// Zero dependencies; requires Node >= 22. Exits non-zero on failure.

import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { createServer } from 'node:http'
import { fileURLToPath } from 'node:url'
import path from 'node:path'
import process from 'node:process'

const BRIDGE = path.join(path.dirname(fileURLToPath(import.meta.url)), 'discord-channel.mjs')
const TOKEN = 'test-token'
const WS_GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'

// ── Minimal websocket server (text frames only, small payloads) ───────

function acceptKey(key) {
  return createHash('sha1').update(key + WS_GUID).digest('base64')
}

function encodeFrame(text) {
  const payload = Buffer.from(text, 'utf8')
  let header
  if (payload.length < 126) {
    header = Buffer.from([0x81, payload.length])
  } else {
    header = Buffer.alloc(4)
    header[0] = 0x81
    header[1] = 126
    header.writeUInt16BE(payload.length, 2)
  }
  return Buffer.concat([header, payload])
}

// Streaming decoder for masked client frames; calls onMessage(text) per
// text frame and onClose() on close frames.
function frameDecoder(onMessage, onClose) {
  let buf = Buffer.alloc(0)
  return (data) => {
    buf = Buffer.concat([buf, data])
    for (;;) {
      if (buf.length < 2) return
      const opcode = buf[0] & 0x0f
      const masked = (buf[1] & 0x80) !== 0
      let len = buf[1] & 0x7f
      let offset = 2
      if (len === 126) {
        if (buf.length < 4) return
        len = buf.readUInt16BE(2)
        offset = 4
      } else if (len === 127) {
        if (buf.length < 10) return
        len = Number(buf.readBigUInt64BE(2))
        offset = 10
      }
      const maskLen = masked ? 4 : 0
      if (buf.length < offset + maskLen + len) return
      const mask = masked ? buf.subarray(offset, offset + 4) : null
      const payload = Buffer.from(buf.subarray(offset + maskLen, offset + maskLen + len))
      if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4]
      buf = buf.subarray(offset + maskLen + len)
      if (opcode === 0x8) onClose?.()
      else if (opcode === 0x1) onMessage(payload.toString('utf8'))
      // ping/pong/binary ignored — the bridge doesn't send them.
    }
  }
}

function startMockGateway() {
  const clients = []
  const received = [] // parsed payloads from the bridge
  const waiters = []
  const connectionWaiters = []
  const server = createServer()
  server.on('upgrade', (req, socket) => {
    socket.write(
      'HTTP/1.1 101 Switching Protocols\r\n' +
        'Upgrade: websocket\r\nConnection: Upgrade\r\n' +
        `Sec-WebSocket-Accept: ${acceptKey(req.headers['sec-websocket-key'])}\r\n\r\n`,
    )
    const client = { socket, send: (obj) => socket.write(encodeFrame(JSON.stringify(obj))) }
    clients.push(client)
    while (connectionWaiters.length) connectionWaiters.shift()(client)
    socket.on(
      'data',
      frameDecoder(
        (text) => {
          const payload = JSON.parse(text)
          received.push(payload)
          for (const [i, w] of waiters.entries()) {
            if (w.predicate(payload)) {
              waiters.splice(i, 1)
              w.resolve(payload)
              break
            }
          }
        },
        () => socket.end(),
      ),
    )
    socket.on('error', () => {})
  })
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      resolve({
        server,
        clients,
        received,
        port: server.address().port,
        waitForClient(timeoutMs = 3000) {
          if (clients.length > 0) return Promise.resolve(clients[clients.length - 1])
          return new Promise((res, rej) => {
            const timer = setTimeout(
              () => rej(new Error('timed out waiting for a gateway connection')),
              timeoutMs,
            )
            connectionWaiters.push((c) => {
              clearTimeout(timer)
              res(c)
            })
          })
        },
        // Await the next (or an already-received) payload matching predicate.
        expectPayload(predicate, label, timeoutMs = 3000) {
          const already = received.find(predicate)
          if (already) return Promise.resolve(already)
          return new Promise((res, rej) => {
            const timer = setTimeout(
              () => rej(new Error(`timed out waiting for gateway payload: ${label}`)),
              timeoutMs,
            )
            waiters.push({
              predicate,
              resolve: (p) => {
                clearTimeout(timer)
                res(p)
              },
            })
          })
        },
      })
    })
  })
}

// ── Mock Discord REST API ─────────────────────────────────────────────

function startMockRest() {
  const requests = []
  const server = createServer((req, res) => {
    let body = ''
    req.on('data', (c) => (body += c))
    req.on('end', () => {
      requests.push({
        method: req.method,
        url: req.url,
        auth: req.headers.authorization,
        body: body ? JSON.parse(body) : null,
      })
      if (req.method === 'POST' && /\/channels\/[^/]+\/messages$/.test(req.url)) {
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify({ id: 'msg-999' }))
      } else if (req.method === 'PUT' && req.url.includes('/reactions/')) {
        res.writeHead(204)
        res.end()
      } else if (req.method === 'GET' && req.url.includes('/messages?')) {
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(
          JSON.stringify([
            {
              id: '7',
              author: { username: 'karl', id: '42' },
              timestamp: 't1',
              content: 'earlier message',
              attachments: [],
            },
          ]),
        )
      } else {
        res.writeHead(404)
        res.end('{}')
      }
    })
  })
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () =>
      resolve({ server, requests, port: server.address().port }),
    )
  })
}

// ── Test harness ──────────────────────────────────────────────────────

let failures = 0
function check(cond, label) {
  if (cond) console.log(`  ok: ${label}`)
  else {
    failures++
    console.error(`  FAIL: ${label}`)
  }
}

async function main() {
  const gateway = await startMockGateway()
  const rest = await startMockRest()

  const child = spawn(process.execPath, [BRIDGE], {
    env: {
      ...process.env,
      DISCORD_BOT_TOKEN: TOKEN,
      DISCORD_GATEWAY_URL: `ws://127.0.0.1:${gateway.port}/`,
      DISCORD_API_BASE: `http://127.0.0.1:${rest.port}/api/v10`,
      DISCORD_ALLOWED_USER_IDS: '42',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  })
  child.stderr.on('data', (d) => process.env.TEST_VERBOSE && process.stderr.write(d))

  // stdout: newline-delimited JSON-RPC from the bridge.
  const fromBridge = []
  const stdoutWaiters = []
  let outBuf = ''
  child.stdout.on('data', (data) => {
    outBuf += data
    let nl
    while ((nl = outBuf.indexOf('\n')) !== -1) {
      const line = outBuf.slice(0, nl)
      outBuf = outBuf.slice(nl + 1)
      if (!line.trim()) continue
      const msg = JSON.parse(line)
      fromBridge.push(msg)
      for (const [i, w] of stdoutWaiters.entries()) {
        if (w.predicate(msg)) {
          stdoutWaiters.splice(i, 1)
          w.resolve(msg)
          break
        }
      }
    }
  })
  function expectStdout(predicate, label, timeoutMs = 3000) {
    const already = fromBridge.find(predicate)
    if (already) return Promise.resolve(already)
    return new Promise((res, rej) => {
      const timer = setTimeout(
        () => rej(new Error(`timed out waiting for bridge message: ${label}`)),
        timeoutMs,
      )
      stdoutWaiters.push({
        predicate,
        resolve: (m) => {
          clearTimeout(timer)
          res(m)
        },
      })
    })
  }
  let nextId = 1
  function request(method, params) {
    const id = nextId++
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n')
    return expectStdout((m) => m.id === id, `response to ${method}`)
  }

  console.log('MCP handshake')
  const init = await request('initialize', {
    protocolVersion: '2025-06-18',
    capabilities: {},
    clientInfo: { name: 'test', version: '0' },
  })
  check(
    init.result?.capabilities?.experimental?.['grok/channel'] !== undefined,
    'initialize declares the grok/channel capability',
  )
  check(
    typeof init.result?.instructions === 'string' && init.result.instructions.includes('send_message'),
    'initialize carries reply instructions',
  )
  child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n')

  const tools = await request('tools/list', {})
  const toolNames = (tools.result?.tools ?? []).map((t) => t.name).sort()
  check(
    JSON.stringify(toolNames) === JSON.stringify(['add_reaction', 'read_messages', 'send_message']),
    `tools/list returns the three tools (got: ${toolNames.join(', ')})`,
  )

  console.log('gateway handshake')
  const gw = await gateway.waitForClient()
  check(gateway.clients.length === 1, 'bridge opened one gateway connection')
  // Discord order: server sends HELLO first, client answers IDENTIFY.
  // The fast heartbeat interval lets the test observe beats below.
  gw.send({ op: 10, d: { heartbeat_interval: 150 } })
  const identify = await gateway.expectPayload((p) => p.op === 2, 'IDENTIFY')
  check(identify.d.token === TOKEN, 'IDENTIFY carries the bot token')
  check(identify.d.intents === 37377, `IDENTIFY intents include MESSAGE_CONTENT (got ${identify.d.intents})`)
  gw.send({
    op: 0,
    s: 1,
    t: 'READY',
    d: { session_id: 'sess-1', resume_gateway_url: '', user: { id: 'BOT', username: 'grok' } },
  })

  console.log('inbound message gating')
  gw.send({
    op: 0,
    s: 2,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm1',
      channel_id: 'dm-chan',
      content: 'hello from dm',
      author: { id: '42', username: 'karl' },
    },
  })
  const ev = await expectStdout(
    (m) => m.method === 'notifications/grok/channel',
    'channel notification for allowed DM',
  )
  check(ev.params.content === 'hello from dm', 'DM content forwarded verbatim')
  check(ev.params.meta.author_id === '42', 'meta carries author_id')
  check(ev.params.meta.dm === 'true', 'meta marks DMs')
  check(ev.params.meta.channel_id === 'dm-chan', 'meta carries channel_id')

  // Disallowed sender: no notification may arrive.
  gw.send({
    op: 0,
    s: 3,
    t: 'MESSAGE_CREATE',
    d: { id: 'm2', channel_id: 'dm-chan', content: 'intruder', author: { id: '666', username: 'x' } },
  })
  // Guild message without a mention: dropped.
  gw.send({
    op: 0,
    s: 4,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm3',
      guild_id: 'g1',
      channel_id: 'c1',
      content: 'no mention',
      author: { id: '42', username: 'karl' },
      mentions: [],
    },
  })
  // Guild message with a mention: forwarded, mention stripped.
  gw.send({
    op: 0,
    s: 5,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm4',
      guild_id: 'g1',
      channel_id: 'c1',
      content: '<@BOT> fix the build',
      author: { id: '42', username: 'karl' },
      mentions: [{ id: 'BOT' }],
    },
  })
  const guildEv = await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_id === 'm4',
    'channel notification for mentioned guild message',
  )
  check(guildEv.params.content === 'fix the build', 'bot mention stripped from content')
  check(guildEv.params.meta.guild_id === 'g1', 'meta carries guild_id')
  const forwarded = fromBridge.filter((m) => m.method === 'notifications/grok/channel')
  check(
    forwarded.length === 2 &&
      !forwarded.some((m) => ['m2', 'm3'].includes(m.params.meta.message_id)),
    'disallowed sender and unmentioned guild message were dropped',
  )

  console.log('heartbeat')
  await gateway.expectPayload((p) => p.op === 1, 'heartbeat', 2000)
  check(true, 'bridge heartbeats on the HELLO interval')

  console.log('reply tools')
  const sendRes = await request('tools/call', {
    name: 'send_message',
    arguments: { channel_id: 'c1', content: 'on it', reply_to_message_id: 'm4' },
  })
  check(
    sendRes.result?.content?.[0]?.text?.includes('msg-999'),
    'send_message returns the posted message id',
  )
  const post = rest.requests.find((r) => r.method === 'POST')
  check(post?.url === '/api/v10/channels/c1/messages', 'send_message hit the channel messages endpoint')
  check(post?.body?.content === 'on it', 'send_message posted the content')
  check(post?.body?.message_reference?.message_id === 'm4', 'reply reference attached')
  check(post?.auth === `Bot ${TOKEN}`, 'REST calls authenticate with the bot token')

  const longRes = await request('tools/call', {
    name: 'send_message',
    arguments: { channel_id: 'c1', content: 'line\n'.repeat(900) }, // 4500 chars
  })
  const posts = rest.requests.filter((r) => r.method === 'POST')
  check(posts.length === 4, `long content chunked into multiple messages (got ${posts.length - 1} chunks)`)
  check(
    posts.slice(1).every((p) => p.body.content.length <= 2000),
    'every chunk respects the 2000-char limit',
  )
  check(longRes.result?.content?.[0]?.text?.includes('split into 3'), 'chunking reported to the agent')

  const reactRes = await request('tools/call', {
    name: 'add_reaction',
    arguments: { channel_id: 'c1', message_id: 'm4', emoji: '\u{1F44D}' },
  })
  check(reactRes.result?.content?.[0]?.text === 'reaction added', 'add_reaction succeeds')
  check(
    rest.requests.some((r) => r.method === 'PUT' && r.url.includes('/reactions/')),
    'add_reaction hit the reactions endpoint',
  )

  const readRes = await request('tools/call', {
    name: 'read_messages',
    arguments: { channel_id: 'c1', limit: 5 },
  })
  check(
    readRes.result?.content?.[0]?.text?.includes('earlier message'),
    'read_messages returns simplified history',
  )

  console.log('teardown')
  child.stdin.end()
  const exited = await new Promise((resolve) => {
    const t = setTimeout(() => resolve(false), 2000)
    child.once('exit', () => {
      clearTimeout(t)
      resolve(true)
    })
  })
  check(exited, 'bridge exits when stdin closes')
  child.kill('SIGKILL')
  gateway.server.close()
  rest.server.close()

  console.log(failures === 0 ? '\nall checks passed' : `\n${failures} check(s) FAILED`)
  process.exit(failures === 0 ? 0 : 1)
}

main().catch((err) => {
  console.error('test harness error:', err)
  process.exit(1)
})
