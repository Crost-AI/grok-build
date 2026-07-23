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
          // ACK heartbeats (op 1) so the bridge does not recycle mid-suite
          // during long waits (mention-window expiry is 2s+; HELLO interval is 150ms).
          if (payload.op === 1) {
            client.send({ op: 11 })
          }
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
      let parsed = null
      try {
        parsed = body ? JSON.parse(body) : null
      } catch {
        /* multipart or non-JSON body — keep raw only */
      }
      requests.push({
        method: req.method,
        url: req.url,
        auth: req.headers.authorization,
        contentType: req.headers['content-type'] ?? '',
        body: parsed,
        raw: body,
      })
      if (req.method === 'GET' && req.url.startsWith('/cdn/')) {
        res.writeHead(200, { 'Content-Type': 'text/plain' })
        res.end('attachment-bytes-123')
      } else if (req.method === 'POST' && /\/channels\/[^/]+\/threads$/.test(req.url)) {
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(
          JSON.stringify({
            id: 'thr-new',
            parent_id: req.url.match(/\/channels\/([^/]+)\/threads/)?.[1] ?? null,
            name: body ? JSON.parse(body).name : 'thread',
            type: 11,
          }),
        )
      } else if (req.method === 'POST' && /\/channels\/[^/]+\/messages$/.test(req.url)) {
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify({ id: 'msg-999' }))
      } else if (req.method === 'GET' && /\/channels\/[^/]+\/messages\/pm1$/.test(req.url)) {
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(
          JSON.stringify({
            id: 'pm1',
            poll: {
              question: { text: 'Ship it?' },
              allow_multiselect: false,
              expiry: '2026-07-23T00:00:00Z',
              answers: [
                { answer_id: 1, poll_media: { text: 'Yes' } },
                { answer_id: 2, poll_media: { text: 'No' } },
              ],
              results: {
                is_finalized: false,
                answer_counts: [
                  { id: 1, count: 3, me_voted: false },
                  { id: 2, count: 1, me_voted: false },
                ],
              },
            },
          }),
        )
      } else if (req.method === 'GET' && req.url.includes('/polls/pm1/answers/')) {
        const answerId = req.url.split('/answers/')[1].split('?')[0]
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(
          JSON.stringify({
            users: answerId === '1' ? [{ id: '42', username: 'karl' }] : [],
          }),
        )
      } else if (req.method === 'POST' && req.url.includes('/polls/pm1/expire')) {
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify({ id: 'pm1' }))
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
      } else if (req.method === 'GET' && /\/channels\/[^/]+$/.test(req.url)) {
        // Channel/thread lookup for allowlist inheritance fallback.
        const id = req.url.split('/').pop()
        const parents = { thr1: 'c1', thr2: 'c-other', 'thr-new': 'c1' }
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(
          JSON.stringify({
            id,
            parent_id: parents[id] ?? null,
            type: parents[id] ? 11 : 0,
          }),
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

  // Explicit env — do NOT inherit host DISCORD_* (a live Grok session often
  // exports DISCORD_REQUIRE_MENTION=false / real channel ids, which break the suite).
  const child = spawn(process.execPath, [BRIDGE], {
    env: {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      DISCORD_BOT_TOKEN: TOKEN,
      DISCORD_GATEWAY_URL: `ws://127.0.0.1:${gateway.port}/`,
      DISCORD_API_BASE: `http://127.0.0.1:${rest.port}/api/v10`,
      DISCORD_ALLOWED_USER_IDS: '42',
      DISCORD_ALLOWED_BOT_IDS: '777',
      DISCORD_REQUIRE_MENTION: 'true',
      DISCORD_MENTION_WINDOW_SECONDS: '2',
      DISCORD_ATTACHMENT_HOSTS: '127.0.0.1',
      // Allowlisted parents/channels used by the suite. Threads inherit via
      // parent (thr1 under c1 ok; thr2 under c-other drops).
      DISCORD_CHANNEL_IDS: 'c1,c2,c3',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  })
  let stderrBuf = ''
  child.stderr.on('data', (d) => {
    stderrBuf += d
    if (process.env.TEST_VERBOSE) process.stderr.write(d)
  })

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
  const commands = init.result?.capabilities?.experimental?.['grok/channel']?.commands
  check(
    commands?.reply_tool === 'send_message' &&
      commands?.target_meta === 'channel_id' &&
      commands?.target_arg === 'channel_id' &&
      commands?.content_arg === 'content',
    'capability value carries the host slash-command reply descriptor',
  )
  check(
    typeof init.result?.instructions === 'string' && init.result.instructions.includes('send_message'),
    'initialize carries reply instructions',
  )
  child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n')

  const tools = await request('tools/list', {})
  const toolNames = (tools.result?.tools ?? []).map((t) => t.name).sort()
  check(
    JSON.stringify(toolNames) ===
      JSON.stringify([
        'add_reaction',
        'create_poll',
        'create_thread',
        'end_poll',
        'read_attachment',
        'read_messages',
        'read_poll',
        'send_file',
        'send_message',
      ]),
    `tools/list returns the nine tools (got: ${toolNames.join(', ')})`,
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
  // Discord sends one GUILD_CREATE per guild after READY; it carries the
  // bot's managed role, which the mention gate must honor.
  gw.send({
    op: 0,
    s: 2,
    t: 'GUILD_CREATE',
    d: { id: 'g1', roles: [{ id: 'R1', name: 'grok', tags: { bot_id: 'BOT' } }] },
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
  // Mentioning the bot's managed ROLE (what Discord's picker often
  // inserts for "@botname") must count as a mention too. Fresh channel
  // (c2) so m4's continuation window can't mask a broken role gate.
  gw.send({
    op: 0,
    s: 6,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm5',
      guild_id: 'g1',
      channel_id: 'c2',
      content: '<@&R1> status?',
      author: { id: '42', username: 'karl' },
      mentions: [],
      mention_roles: ['R1'],
    },
  })
  const roleEv = await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_id === 'm5',
    'channel notification for role-mentioned guild message',
  )
  check(roleEv.params.content === 'status?', 'role mention counts and is stripped from content')
  // Bot senders: dropped unless in DISCORD_ALLOWED_BOT_IDS.
  gw.send({
    op: 0,
    s: 7,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm6',
      channel_id: 'dm-chan',
      content: 'unlisted bot',
      author: { id: '888', username: 'otherbot', bot: true },
    },
  })
  gw.send({
    op: 0,
    s: 8,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm7',
      channel_id: 'dm-chan',
      content: 'claude checking in',
      author: { id: '777', username: 'claude', bot: true },
    },
  })
  const botEv = await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_id === 'm7',
    'channel notification for allowlisted bot',
  )
  check(botEv.params.meta.bot === 'true', 'bot senders are marked with bot="true" meta')

  console.log('addressed-without-mention paths')
  // A Discord reply to one of the bot's messages counts as addressing it.
  gw.send({
    op: 0,
    s: 9,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm8',
      guild_id: 'g1',
      channel_id: 'c3',
      content: 'replying to your last answer',
      author: { id: '42', username: 'karl' },
      mentions: [],
      referenced_message: { id: 'old-bot-msg', author: { id: 'BOT' } },
    },
  })
  await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_id === 'm8',
    'reply-to-bot forwarded without a mention',
  )
  check(true, 'reply to a bot message counts as addressing the bot')
  // Continuation window: after a mentioned message, the same sender's
  // unmentioned follow-up in the same channel flows (split-message case).
  gw.send({
    op: 0,
    s: 10,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm9a',
      guild_id: 'g1',
      channel_id: 'c1',
      content: '<@BOT> part 1 of a long briefing',
      author: { id: '42', username: 'karl' },
      mentions: [{ id: 'BOT' }],
    },
  })
  gw.send({
    op: 0,
    s: 11,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm9b',
      guild_id: 'g1',
      channel_id: 'c1',
      content: 'part 2, no mention',
      author: { id: '42', username: 'karl' },
      mentions: [],
    },
  })
  await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_id === 'm9b',
    'continuation chunk forwarded within the window',
  )
  check(true, 'unmentioned follow-up within the window is forwarded')
  // After the window expires, unmentioned messages drop again.
  await new Promise((r) => setTimeout(r, 2600))
  gw.send({
    op: 0,
    s: 12,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm10',
      guild_id: 'g1',
      channel_id: 'c1',
      content: 'late, no mention',
      author: { id: '42', username: 'karl' },
      mentions: [],
    },
  })
  await new Promise((r) => setTimeout(r, 300))
  const forwarded = fromBridge.filter((m) => m.method === 'notifications/grok/channel')
  const forwardedIds = forwarded.map((m) => m.params.meta.message_id)
  // Assert the drops by id (not a brittle total count — suite grows with tools).
  check(
    !forwardedIds.some((id) => ['m2', 'm3', 'm6', 'm10'].includes(id)),
    'disallowed sender, unmentioned guild message, unlisted bot, and expired-window follow-up were dropped',
  )
  check(
    stderrBuf.includes('(id 666)') && stderrBuf.includes('not in DISCORD_ALLOWED_USER_IDS'),
    'sender-gate drop is logged to stderr with the sender id',
  )

  console.log('thread inheritance under DISCORD_CHANNEL_IDS')
  // Parent c1 is allowlisted; thread thr1 is not listed but parent is c1.
  // Unique message ids (m11+) — earlier suite already used m8/m9*.
  gw.send({
    op: 0,
    s: 20,
    t: 'THREAD_CREATE',
    d: { id: 'thr1', parent_id: 'c1', guild_id: 'g1', name: 'test-thread' },
  })
  // Let THREAD_CREATE land before MESSAGE_CREATE (map + message chain).
  await new Promise((r) => setTimeout(r, 50))
  gw.send({
    op: 0,
    s: 21,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm11',
      guild_id: 'g1',
      channel_id: 'thr1',
      content: '<@BOT> testing in thread',
      author: { id: '42', username: 'karl' },
      mentions: [{ id: 'BOT' }],
    },
  })
  const thrEv = await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_id === 'm11',
    'channel notification for thread under allowlisted parent',
  )
  check(thrEv.params.content === 'testing in thread', 'thread mention stripped')
  check(thrEv.params.meta.channel_id === 'thr1', 'meta channel_id is the thread id')
  check(
    thrEv.params.meta.parent_channel_id === 'c1',
    'meta carries parent_channel_id for thread messages',
  )
  // Thread under a non-allowlisted parent must drop.
  gw.send({
    op: 0,
    s: 22,
    t: 'THREAD_CREATE',
    d: { id: 'thr2', parent_id: 'c-other', guild_id: 'g1', name: 'other-thread' },
  })
  gw.send({
    op: 0,
    s: 23,
    t: 'MESSAGE_CREATE',
    d: {
      id: 'm12',
      guild_id: 'g1',
      channel_id: 'thr2',
      content: '<@BOT> should drop',
      author: { id: '42', username: 'karl' },
      mentions: [{ id: 'BOT' }],
    },
  })
  await new Promise((r) => setTimeout(r, 150))
  check(
    !fromBridge.some(
      (m) =>
        m.method === 'notifications/grok/channel' && m.params.meta.message_id === 'm12',
    ),
    'thread under non-allowlisted parent was dropped',
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

  const pollRes = await request('tools/call', {
    name: 'create_poll',
    arguments: {
      channel_id: 'c1',
      question: 'Ship the tip feature tonight?',
      answers: [
        { text: 'Yes', emoji: '✅' },
        'No',
        { text: 'Later', emoji: 'clock:123' },
      ],
      duration: 48,
      allow_multiselect: false,
      content: 'quick vote',
    },
  })
  check(
    pollRes.result?.content?.[0]?.text?.includes('poll created') &&
      pollRes.result?.content?.[0]?.text?.includes('msg-999'),
    'create_poll returns the posted message id',
  )
  const pollPost = rest.requests
    .filter((r) => r.method === 'POST' && r.url === '/api/v10/channels/c1/messages')
    .find((r) => r.body?.poll)
  check(Boolean(pollPost), 'create_poll hit the channel messages endpoint with a poll body')
  check(pollPost?.body?.content === 'quick vote', 'create_poll optional caption attached')
  check(pollPost?.body?.poll?.question?.text === 'Ship the tip feature tonight?', 'poll question set')
  check(pollPost?.body?.poll?.duration === 48, 'poll duration in hours')
  check(pollPost?.body?.poll?.allow_multiselect === false, 'poll multiselect default/false')
  check(pollPost?.body?.poll?.answers?.length === 3, 'poll has three answers')
  check(
    pollPost?.body?.poll?.answers?.[0]?.poll_media?.emoji?.name === '✅' &&
      pollPost?.body?.poll?.answers?.[2]?.poll_media?.emoji?.id === '123',
    'poll answer emojis normalized (unicode + custom name:id)',
  )

  const badPoll = await request('tools/call', {
    name: 'create_poll',
    arguments: { channel_id: 'c1', question: 'only one?', answers: ['A'] },
  })
  check(
    badPoll.result?.isError === true &&
      badPoll.result?.content?.[0]?.text?.includes('2–10'),
    'create_poll rejects fewer than 2 answers',
  )

  console.log('create_thread')
  const thrRes = await request('tools/call', {
    name: 'create_thread',
    arguments: {
      parent_channel_id: 'c1',
      name: 'PR · #700 · test <@999> @everyone',
      message: 'workstream open',
    },
  })
  const thrText = thrRes.result?.content?.[0]?.text ?? ''
  check(
    thrText.includes('thread created') && thrText.includes('thr-new'),
    'create_thread returns new thread id',
  )
  check(
    thrText.includes('parent c1') && thrText.includes('first message id msg-999'),
    'create_thread posts optional first message',
  )
  const thrPost = rest.requests.find(
    (r) => r.method === 'POST' && r.url === '/api/v10/channels/c1/threads',
  )
  check(Boolean(thrPost), 'create_thread hit parent /threads endpoint')
  check(thrPost?.body?.type === 11, 'public thread type 11')
  check(thrPost?.body?.auto_archive_duration === 1440, '24h auto-archive (1440 min)')
  check(
    thrPost?.body?.name === 'PR · #700 · test',
    `thread name sanitized (got ${JSON.stringify(thrPost?.body?.name)})`,
  )
  const thrMsg = rest.requests.find(
    (r) => r.method === 'POST' && r.url === '/api/v10/channels/thr-new/messages',
  )
  check(thrMsg?.body?.content === 'workstream open', 'first message posted into new thread')

  const thrDenied = await request('tools/call', {
    name: 'create_thread',
    arguments: { parent_channel_id: 'c-other', name: 'nope' },
  })
  check(
    thrDenied.result?.isError === true &&
      thrDenied.result?.content?.[0]?.text?.includes('DISCORD_CHANNEL_IDS'),
    'create_thread rejects non-allowlisted parent',
  )

  const thrEmpty = await request('tools/call', {
    name: 'create_thread',
    arguments: { parent_channel_id: 'c1', name: '<@123> @everyone' },
  })
  check(
    thrEmpty.result?.isError === true &&
      thrEmpty.result?.content?.[0]?.text?.includes('non-empty name'),
    'create_thread rejects empty name after mention strip',
  )

  console.log('file attachments')
  const { writeFile: writeTmp, readFile: readTmp } = await import('node:fs/promises')
  const osmod = await import('node:os')
  const tmpFile = path.join(osmod.tmpdir(), `discord-sendfile-test-${process.pid}.txt`)
  await writeTmp(tmpFile, 'hello attachment content')
  const fileRes = await request('tools/call', {
    name: 'send_file',
    arguments: { channel_id: 'c1', file_path: tmpFile, caption: 'here you go' },
  })
  check(
    fileRes.result?.content?.[0]?.text?.includes('msg-999'),
    `send_file returns the posted message id (got: ${fileRes.result?.content?.[0]?.text})`,
  )
  const upload = api_last_multipart()
  function api_last_multipart() {
    return rest.requests.findLast(
      (r) => r.method === 'POST' && r.contentType.startsWith('multipart/form-data'),
    )
  }
  check(Boolean(upload), 'send_file posts multipart/form-data')
  check(
    Boolean(upload) &&
      upload.raw.includes('payload_json') &&
      upload.raw.includes('here you go') &&
      upload.raw.includes('discord-sendfile-test-'),
    'multipart body carries payload_json, caption, and filename',
  )
  const readRes2 = await request('tools/call', {
    name: 'read_attachment',
    arguments: { url: `http://127.0.0.1:${rest.port}/cdn/incoming-log.txt` },
  })
  let saved = null
  try {
    saved = JSON.parse(readRes2.result?.content?.[0]?.text ?? '')
  } catch {
    /* checked below */
  }
  check(
    saved && saved.bytes === 'attachment-bytes-123'.length,
    `read_attachment reports the downloaded size (got: ${readRes2.result?.content?.[0]?.text})`,
  )
  check(
    saved && (await readTmp(saved.path, 'utf8')) === 'attachment-bytes-123',
    'read_attachment saves the attachment content to the reported path',
  )
  const blocked = await request('tools/call', {
    name: 'read_attachment',
    arguments: { url: 'http://evil.example/x.txt' },
  })
  check(
    blocked.result?.isError === true &&
      blocked.result?.content?.[0]?.text?.includes('only fetches Discord CDN'),
    'read_attachment refuses non-CDN hosts',
  )

  console.log('polls')
  const pollRead = await request('tools/call', {
    name: 'read_poll',
    arguments: { channel_id: 'c1', message_id: 'pm1' },
  })
  let pollData = null
  try {
    pollData = JSON.parse(pollRead.result?.content?.[0]?.text ?? '')
  } catch {
    /* checked below */
  }
  check(
    pollData && pollData.question === 'Ship it?' && pollData.answers?.[0]?.count === 3,
    `read_poll returns question and counts (got: ${pollRead.result?.content?.[0]?.text?.slice(0, 80)})`,
  )
  check(
    pollData && pollData.answers?.[0]?.voters?.[0] === 'karl' && pollData.answers?.[1]?.voters?.length === 0,
    'read_poll includes per-answer voters',
  )
  const pollEnd = await request('tools/call', {
    name: 'end_poll',
    arguments: { channel_id: 'c1', message_id: 'pm1' },
  })
  check(pollEnd.result?.content?.[0]?.text === 'poll ended; message id pm1', 'end_poll hits the expire endpoint')
  check(
    rest.requests.some((r) => r.method === 'POST' && r.url.includes('/polls/pm1/expire')),
    'expire request recorded',
  )

  console.log('addressed-awareness (mention requirement off)')
  const clientsBefore = gateway.clients.length
  const child3 = spawn(process.execPath, [BRIDGE], {
    env: {
      ...process.env,
      DISCORD_BOT_TOKEN: TOKEN,
      DISCORD_GATEWAY_URL: `ws://127.0.0.1:${gateway.port}/`,
      DISCORD_API_BASE: `http://127.0.0.1:${rest.port}/api/v10`,
      DISCORD_ALLOWED_USER_IDS: '42',
      DISCORD_REQUIRE_MENTION: 'false',
      DISCORD_MENTION_WINDOW_SECONDS: '0',
    },
    stdio: ['pipe', 'pipe', 'ignore'],
  })
  const fromChild3 = []
  const child3Waiters = []
  let out3 = ''
  child3.stdout.on('data', (data) => {
    out3 += data
    let nl
    while ((nl = out3.indexOf('\n')) !== -1) {
      const line = out3.slice(0, nl)
      out3 = out3.slice(nl + 1)
      if (!line.trim()) continue
      const msg = JSON.parse(line)
      fromChild3.push(msg)
      for (const [i, w] of child3Waiters.entries()) {
        if (w.predicate(msg)) {
          child3Waiters.splice(i, 1)
          w.resolve(msg)
          break
        }
      }
    }
  })
  function expectChild3(predicate, label, timeoutMs = 3000) {
    const already = fromChild3.find(predicate)
    if (already) return Promise.resolve(already)
    return new Promise((res, rej) => {
      const timer = setTimeout(
        () => rej(new Error(`timed out waiting for child3: ${label}`)),
        timeoutMs,
      )
      child3Waiters.push({
        predicate,
        resolve: (m) => {
          clearTimeout(timer)
          res(m)
        },
      })
    })
  }
  child3.stdin.write(
    JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'initialize',
      params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 't3', version: '0' } },
    }) + '\n',
  )
  await expectChild3((m) => m.id === 1, 'initialize response')
  {
    const deadline = Date.now() + 3000
    while (gateway.clients.length <= clientsBefore) {
      if (Date.now() > deadline) throw new Error('timed out waiting for child3 gateway connection')
      await new Promise((r) => setTimeout(r, 50))
    }
  }
  const gw3 = gateway.clients[gateway.clients.length - 1]
  gw3.send({ op: 10, d: { heartbeat_interval: 5000 } })
  await new Promise((r) => setTimeout(r, 150)) // let child3's IDENTIFY flow
  gw3.send({
    op: 0,
    s: 1,
    t: 'READY',
    d: { session_id: 'sess-3', resume_gateway_url: '', user: { id: 'BOT', username: 'grok' } },
  })
  // Open chatter (no mentions): forwarded with addressed="none".
  gw3.send({
    op: 0,
    s: 2,
    t: 'MESSAGE_CREATE',
    d: { id: 'a1', guild_id: 'g1', channel_id: 'c1', content: 'thinking out loud', author: { id: '42', username: 'karl' }, mentions: [] },
  })
  const chatter = await expectChild3(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_id === 'a1',
    'chatter forwarded',
  )
  check(chatter.params.meta.addressed === 'none', 'open chatter carries addressed="none"')
  // Message @ someone else: addressed="other".
  gw3.send({
    op: 0,
    s: 3,
    t: 'MESSAGE_CREATE',
    d: { id: 'a2', guild_id: 'g1', channel_id: 'c1', content: '<@555> can you check this?', author: { id: '42', username: 'karl' }, mentions: [{ id: '555' }] },
  })
  const toOther = await expectChild3(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_id === 'a2',
    'other-addressed forwarded',
  )
  check(toOther.params.meta.addressed === 'other', 'message @ someone else carries addressed="other"')
  // Message @ the bot: no addressed attribute (directed at you).
  gw3.send({
    op: 0,
    s: 4,
    t: 'MESSAGE_CREATE',
    d: { id: 'a3', guild_id: 'g1', channel_id: 'c1', content: '<@BOT> and you?', author: { id: '42', username: 'karl' }, mentions: [{ id: 'BOT' }] },
  })
  const toYou = await expectChild3(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_id === 'a3',
    'bot-addressed forwarded',
  )
  check(toYou.params.meta.addressed === undefined, 'bot-directed message has no addressed attribute')
  child3.kill('SIGKILL')

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
