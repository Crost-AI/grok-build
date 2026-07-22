#!/usr/bin/env node
// End-to-end test for slack-channel.mjs: spawns the bridge with a mock
// Slack Socket Mode server (minimal RFC 6455 websocket server) and a
// mock Web API, then drives it over real stdio.
//
//   node plugins/slack/server/slack-channel.test.mjs
//
// Zero dependencies; requires Node >= 22. Exits non-zero on failure.

import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { createServer } from 'node:http'
import { fileURLToPath } from 'node:url'
import path from 'node:path'
import process from 'node:process'

const BRIDGE = path.join(path.dirname(fileURLToPath(import.meta.url)), 'slack-channel.mjs')
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
    }
  }
}

function startMockSocketMode() {
  const clients = []
  const received = [] // parsed payloads from the bridge (envelope acks)
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
              () => rej(new Error('timed out waiting for a socket mode connection')),
              timeoutMs,
            )
            connectionWaiters.push((c) => {
              clearTimeout(timer)
              res(c)
            })
          })
        },
        expectPayload(predicate, label, timeoutMs = 3000) {
          const already = received.find(predicate)
          if (already) return Promise.resolve(already)
          return new Promise((res, rej) => {
            const timer = setTimeout(
              () => rej(new Error(`timed out waiting for socket payload: ${label}`)),
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

// ── Mock Slack Web API ────────────────────────────────────────────────

function startMockWebApi(wsPort) {
  const requests = []
  let tsCounter = 100
  const server = createServer((req, res) => {
    let body = ''
    req.on('data', (c) => (body += c))
    req.on('end', () => {
      const [method, query = ''] = req.url.replace(/^\/api\//, '').split('?')
      let parsed = null
      try {
        parsed = body ? JSON.parse(body) : null
      } catch {
        /* raw byte upload — keep raw only */
      }
      const record = {
        method,
        query,
        auth: req.headers.authorization,
        body: parsed,
        raw: body,
      }
      requests.push(record)
      if (method === 'files-serve/log.txt') {
        res.writeHead(200, { 'Content-Type': 'text/plain' })
        res.end('slack-file-bytes')
        return
      }
      res.writeHead(200, { 'Content-Type': 'application/json' })
      switch (method) {
        case 'apps.connections.open':
          res.end(JSON.stringify({ ok: true, url: `ws://127.0.0.1:${wsPort}/` }))
          break
        case 'files.getUploadURLExternal':
          res.end(
            JSON.stringify({
              ok: true,
              upload_url: `http://127.0.0.1:${server.address().port}/api/upload-bytes`,
              file_id: 'F-NEW',
            }),
          )
          break
        case 'upload-bytes':
          res.end(JSON.stringify({ ok: true }))
          break
        case 'files.completeUploadExternal':
          res.end(JSON.stringify({ ok: true, files: [{ id: 'F-NEW' }] }))
          break
        case 'auth.test':
          res.end(JSON.stringify({ ok: true, user_id: 'UBOT', user: 'grok' }))
          break
        case 'chat.postMessage':
          res.end(JSON.stringify({ ok: true, ts: `T${tsCounter++}` }))
          break
        case 'reactions.add':
          res.end(JSON.stringify({ ok: true }))
          break
        case 'conversations.history':
          res.end(
            JSON.stringify({
              ok: true,
              messages: [{ ts: '9.001', user: 'U42', text: 'earlier message' }],
            }),
          )
          break
        case 'users.info': {
          const id = record.body?.user
          const name = id === 'U42' ? 'karl' : id === 'U666' ? 'intruder' : id
          res.end(
            JSON.stringify({ ok: true, user: { id, name, profile: { display_name: name } } }),
          )
          break
        }
        default:
          res.end(JSON.stringify({ ok: false, error: `unhandled: ${method}` }))
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
  const socketMode = await startMockSocketMode()
  const api = await startMockWebApi(socketMode.port)

  const child = spawn(process.execPath, [BRIDGE], {
    env: {
      ...process.env,
      SLACK_BOT_TOKEN: 'xoxb-test',
      SLACK_APP_TOKEN: 'xapp-test',
      SLACK_API_BASE: `http://127.0.0.1:${api.port}/api`,
      SLACK_ALLOWED_USER_IDS: 'U42',
      SLACK_ALLOWED_BOT_IDS: 'B77',
      SLACK_MENTION_WINDOW_SECONDS: '2',
      SLACK_FILE_HOSTS: '127.0.0.1',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  })
  let stderrBuf = ''
  child.stderr.on('data', (d) => {
    stderrBuf += d
    if (process.env.TEST_VERBOSE) process.stderr.write(d)
  })

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
    typeof init.result?.instructions === 'string' && init.result.instructions.includes('thread_ts'),
    'initialize carries thread-aware reply instructions',
  )
  child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n')

  const tools = await request('tools/list', {})
  const toolNames = (tools.result?.tools ?? []).map((t) => t.name).sort()
  check(
    JSON.stringify(toolNames) ===
      JSON.stringify(['add_reaction', 'read_attachment', 'read_messages', 'send_file', 'send_message']),
    `tools/list returns the five tools (got: ${toolNames.join(', ')})`,
  )

  console.log('socket mode handshake')
  const sm = await socketMode.waitForClient()
  const openReq = api.requests.find((r) => r.method === 'apps.connections.open')
  check(openReq?.auth === 'Bearer xapp-test', 'apps.connections.open uses the app-level token')
  const authReq = api.requests.find((r) => r.method === 'auth.test')
  check(authReq?.auth === 'Bearer xoxb-test', 'auth.test uses the bot token')
  sm.send({ type: 'hello' })

  function sendEvent(id, event) {
    sm.send({ envelope_id: id, type: 'events_api', payload: { event } })
  }

  console.log('inbound message gating')
  sendEvent('e1', {
    type: 'message',
    channel: 'D1',
    channel_type: 'im',
    user: 'U42',
    text: 'hello from dm',
    ts: '1.001',
  })
  const dmEv = await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_ts === '1.001',
    'channel notification for allowed DM',
  )
  check(dmEv.params.content === 'hello from dm', 'DM content forwarded verbatim')
  check(dmEv.params.meta.dm === 'true' && dmEv.params.meta.author === 'karl', 'meta marks DM and resolves author name')
  await socketMode.expectPayload((p) => p.envelope_id === 'e1', 'ack for e1')
  check(true, 'events_api envelopes are acknowledged')

  // Unmentioned channel message: dropped. Mentioned: forwarded, stripped.
  sendEvent('e2', {
    type: 'message',
    channel: 'C1',
    channel_type: 'channel',
    user: 'U42',
    text: 'no mention here',
    ts: '2.001',
  })
  sendEvent('e3', {
    type: 'message',
    channel: 'C1',
    channel_type: 'channel',
    user: 'U42',
    text: '<@UBOT> fix the build',
    ts: '3.001',
  })
  const chanEv = await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_ts === '3.001',
    'channel notification for mentioned channel message',
  )
  check(chanEv.params.content === 'fix the build', 'bot mention stripped from content')
  // Continuation window: unmentioned follow-up from the same sender flows.
  sendEvent('e4', {
    type: 'message',
    channel: 'C1',
    channel_type: 'channel',
    user: 'U42',
    text: 'part 2, no mention',
    ts: '4.001',
  })
  await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_ts === '4.001',
    'continuation chunk forwarded within the window',
  )
  check(true, 'unmentioned follow-up within the window is forwarded')

  // Bots: unlisted dropped, allowlisted forwarded with bot="true".
  sendEvent('e5', {
    type: 'message',
    channel: 'C1',
    channel_type: 'channel',
    user: 'U99',
    bot_id: 'B99',
    text: '<@UBOT> unlisted bot',
    ts: '5.001',
  })
  sendEvent('e6', {
    type: 'message',
    channel: 'C1',
    channel_type: 'channel',
    user: 'U77',
    bot_id: 'B77',
    bot_profile: { name: 'claude' },
    text: '<@UBOT> claude checking in',
    ts: '6.001',
  })
  const botEv = await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_ts === '6.001',
    'channel notification for allowlisted bot',
  )
  check(
    botEv.params.meta.bot === 'true' && botEv.params.meta.author === 'claude',
    'bot senders are marked with bot="true" meta',
  )

  // Sender-gate drop is logged with the sender id.
  sendEvent('e7', {
    type: 'message',
    channel: 'D2',
    channel_type: 'im',
    user: 'U666',
    text: 'intruder dm',
    ts: '7.001',
  })
  await new Promise((r) => setTimeout(r, 300))
  check(
    stderrBuf.includes('(id U666)') && stderrBuf.includes('not in SLACK_ALLOWED_USER_IDS'),
    'sender-gate drop is logged to stderr with the sender id',
  )

  console.log('reply tools + thread addressing')
  const sendRes = await request('tools/call', {
    name: 'send_message',
    arguments: { channel_id: 'C2', content: 'on it' },
  })
  const sentTs = sendRes.result?.content?.[0]?.text?.match(/ts (T\d+)/)?.[1]
  check(Boolean(sentTs), `send_message returns the posted ts (got: ${sendRes.result?.content?.[0]?.text})`)
  const post = api.requests.find((r) => r.method === 'chat.postMessage')
  check(post?.body?.channel === 'C2' && post?.body?.text === 'on it', 'send_message posted the content')
  // A reply in a thread the bot posted in counts as addressed — even
  // with no mention and no prior window in that channel for the sender.
  sendEvent('e8', {
    type: 'message',
    channel: 'C2',
    channel_type: 'channel',
    user: 'U42',
    text: 'thanks, one more thing',
    ts: '8.001',
    thread_ts: sentTs,
  })
  const threadEv = await expectStdout(
    (m) => m.method === 'notifications/grok/channel' && m.params.meta.message_ts === '8.001',
    'thread reply forwarded without a mention',
  )
  check(threadEv.params.meta.thread_ts === sentTs, 'meta carries thread_ts for threaded replies')

  const longRes = await request('tools/call', {
    name: 'send_message',
    arguments: { channel_id: 'C2', content: 'line\n'.repeat(1800) }, // 9000 chars
  })
  const posts = api.requests.filter((r) => r.method === 'chat.postMessage')
  check(
    posts.length === 4 && posts.slice(1).every((p) => p.body.text.length <= 4000),
    `long content chunked into <=4000-char messages (got ${posts.length - 1} chunks)`,
  )
  check(longRes.result?.content?.[0]?.text?.includes('split into 3'), 'chunking reported to the agent')

  const reactRes = await request('tools/call', {
    name: 'add_reaction',
    arguments: { channel_id: 'C1', message_ts: '3.001', emoji: ':thumbsup:' },
  })
  check(reactRes.result?.content?.[0]?.text === 'reaction added', 'add_reaction succeeds')
  const react = api.requests.find((r) => r.method === 'reactions.add')
  check(react?.body?.name === 'thumbsup', 'reaction emoji name is stripped of colons')

  const readRes = await request('tools/call', {
    name: 'read_messages',
    arguments: { channel_id: 'C1', limit: 5 },
  })
  check(
    readRes.result?.content?.[0]?.text?.includes('earlier message'),
    'read_messages returns simplified history',
  )

  // Window expiry: after the window passes, unmentioned messages drop.
  await new Promise((r) => setTimeout(r, 2600))
  sendEvent('e9', {
    type: 'message',
    channel: 'C1',
    channel_type: 'channel',
    user: 'U42',
    text: 'late, no mention',
    ts: '9.501',
  })
  await new Promise((r) => setTimeout(r, 300))
  const forwarded = fromBridge.filter((m) => m.method === 'notifications/grok/channel')
  check(
    forwarded.length === 5 &&
      !forwarded.some((m) => ['2.001', '5.001', '7.001', '9.501'].includes(m.params.meta.message_ts)),
    'unmentioned, unlisted-bot, disallowed-sender, and expired-window messages were dropped',
  )

  console.log('file attachments')
  const { writeFile: writeTmp, readFile: readTmp } = await import('node:fs/promises')
  const osmod = await import('node:os')
  const tmpFile = path.join(osmod.tmpdir(), `slack-sendfile-test-${process.pid}.txt`)
  await writeTmp(tmpFile, 'slack upload payload')
  const fileRes = await request('tools/call', {
    name: 'send_file',
    arguments: { channel_id: 'C1', file_path: tmpFile, caption: 'build log attached' },
  })
  check(
    fileRes.result?.content?.[0]?.text?.includes('F-NEW'),
    `send_file returns the file id (got: ${fileRes.result?.content?.[0]?.text})`,
  )
  const ticketReq = api.requests.find((r) => r.method === 'files.getUploadURLExternal')
  check(
    Boolean(ticketReq) &&
      ticketReq.query.includes('slack-sendfile-test-') &&
      ticketReq.query.includes(`length=${'slack upload payload'.length}`) &&
      ticketReq.auth === 'Bearer xoxb-test',
    'getUploadURLExternal carries filename, length, and the bot token',
  )
  const bytesReq = api.requests.find((r) => r.method === 'upload-bytes')
  check(bytesReq?.raw === 'slack upload payload', 'file bytes posted to the one-time upload URL')
  const completeReq = api.requests.find((r) => r.method === 'files.completeUploadExternal')
  check(
    completeReq?.body?.channel_id === 'C1' &&
      completeReq?.body?.files?.[0]?.id === 'F-NEW' &&
      completeReq?.body?.initial_comment === 'build log attached',
    'completeUploadExternal shares into the channel with the caption',
  )
  const readRes2 = await request('tools/call', {
    name: 'read_attachment',
    arguments: { url: `http://127.0.0.1:${api.port}/api/files-serve/log.txt` },
  })
  let saved = null
  try {
    saved = JSON.parse(readRes2.result?.content?.[0]?.text ?? '')
  } catch {
    /* checked below */
  }
  check(
    saved && (await readTmp(saved.path, 'utf8')) === 'slack-file-bytes',
    `read_attachment saves the file content to the reported path (got: ${readRes2.result?.content?.[0]?.text})`,
  )
  const fileGet = api.requests.find((r) => r.method === 'files-serve/log.txt')
  check(fileGet?.auth === 'Bearer xoxb-test', 'read_attachment authenticates with the bot token')
  const blocked = await request('tools/call', {
    name: 'read_attachment',
    arguments: { url: 'https://evil.example/f.txt' },
  })
  check(
    blocked.result?.isError === true &&
      blocked.result?.content?.[0]?.text?.includes('only fetches Slack file hosts'),
    'read_attachment refuses non-Slack hosts',
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
  socketMode.server.close()
  api.server.close()

  console.log(failures === 0 ? '\nall checks passed' : `\n${failures} check(s) FAILED`)
  process.exit(failures === 0 ? 0 : 1)
}

main().catch((err) => {
  console.error('test harness error:', err)
  process.exit(1)
})
