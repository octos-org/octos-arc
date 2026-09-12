import { useState, useEffect, useRef } from 'react'
import { QRCodeSVG } from 'qrcode.react'
import { myApi } from '../../api'
import type { ProfileConfig, BridgeQrInfo } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
  /** If true, the gateway is currently running (needed for QR polling). */
  isRunning?: boolean
}

export default function WhatsAppTab({ config, onChange, isRunning }: Props) {
  const channel = config.channels.find((c) => c.type === 'whatsapp')
  const enabled = !!channel
  const bridgeUrl = (channel as any)?.bridge_url || ''
  const isManaged = !bridgeUrl || bridgeUrl === 'auto'

  const [qrInfo, setQrInfo] = useState<BridgeQrInfo | null>(null)
  const [qrError, setQrError] = useState<string | null>(null)
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  // Poll for QR code when managed bridge is active and gateway is running
  useEffect(() => {
    if (!enabled || !isManaged || !isRunning) {
      setQrInfo(null)
      setQrError(null)
      return
    }

    const poll = async () => {
      try {
        const info = await myApi.whatsappQr()
        setQrInfo(info)
        setQrError(null)
      } catch {
        // Bridge not started yet or not managed — not an error
        setQrInfo(null)
      }
    }

    poll()
    pollRef.current = setInterval(poll, 2500)
    return () => {
      if (pollRef.current) clearInterval(pollRef.current)
    }
  }, [enabled, isManaged, isRunning])

  const toggle = () => {
    if (enabled) {
      onChange({ ...config, channels: config.channels.filter((c) => c.type !== 'whatsapp') })
    } else {
      // Default to managed mode (empty bridge_url)
      onChange({
        ...config,
        channels: [...config.channels, { type: 'whatsapp', bridge_url: '' }],
      })
    }
  }

  const setMode = (mode: 'managed' | 'external') => {
    const url = mode === 'managed' ? '' : 'ws://localhost:3001'
    const channels = config.channels.map((c) =>
      c.type === 'whatsapp' ? { ...c, bridge_url: url } : c
    )
    onChange({ ...config, channels })
  }

  const updateBridgeUrl = (v: string) => {
    const channels = config.channels.map((c) =>
      c.type === 'whatsapp' ? { ...c, bridge_url: v } : c
    )
    onChange({ ...config, channels })
  }

  const statusColor = {
    waiting: 'text-yellow-400',
    connected: 'text-green-400',
    disconnected: 'text-red-400',
    logged_out: 'text-red-400',
  }

  const statusLabel = {
    waiting: 'Waiting for QR scan...',
    connected: 'Connected',
    disconnected: 'Disconnected',
    logged_out: 'Logged out — delete auth and restart',
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">WhatsApp</p>
        <p>
          Connect WhatsApp to your gateway. In <strong>managed mode</strong>, the server automatically
          runs a WhatsApp bridge — just scan the QR code to pair. In <strong>external mode</strong>,
          you run your own bridge and provide the WebSocket URL.
        </p>
      </div>

      <label className="flex items-center gap-2 cursor-pointer">
        <input
          type="checkbox"
          checked={enabled}
          onChange={toggle}
          className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
        />
        <span className="text-sm text-gray-300">Enable WhatsApp channel</span>
      </label>

      {enabled && (
        <div className="space-y-4">
          {/* Mode toggle */}
          <div className="flex gap-2">
            <button
              onClick={() => setMode('managed')}
              className={`px-3 py-1.5 text-xs rounded-md transition-colors ${
                isManaged
                  ? 'bg-accent text-white'
                  : 'bg-surface-dark text-gray-400 hover:text-gray-300'
              }`}
            >
              Managed (auto)
            </button>
            <button
              onClick={() => setMode('external')}
              className={`px-3 py-1.5 text-xs rounded-md transition-colors ${
                !isManaged
                  ? 'bg-accent text-white'
                  : 'bg-surface-dark text-gray-400 hover:text-gray-300'
              }`}
            >
              External bridge
            </button>
          </div>

          {isManaged ? (
            <div className="space-y-3">
              <p className="text-xs text-gray-500">
                The server will automatically start a WhatsApp bridge when your gateway starts.
                {!isRunning && ' Start your gateway to begin pairing.'}
              </p>

              {isRunning && qrInfo && (
                <div className="bg-surface-dark/50 rounded-lg p-4 border border-gray-700/50">
                  {/* Status */}
                  <div className="flex items-center gap-2 mb-3">
                    <span className={`w-2 h-2 rounded-full ${
                      qrInfo.status === 'connected' ? 'bg-green-400' :
                      qrInfo.status === 'waiting' ? 'bg-yellow-400 animate-pulse' :
                      'bg-red-400'
                    }`} />
                    <span className={`text-xs font-medium ${statusColor[qrInfo.status]}`}>
                      {statusLabel[qrInfo.status]}
                    </span>
                  </div>

                  {/* QR Code */}
                  {qrInfo.qr && qrInfo.status === 'waiting' && (
                    <div className="flex flex-col items-center gap-3">
                      <div className="bg-white p-3 rounded-lg">
                        <QRCodeSVG value={qrInfo.qr} size={200} />
                      </div>
                      <p className="text-xs text-gray-500 text-center">
                        Open WhatsApp on your phone, go to <strong>Linked Devices</strong>,
                        and scan this QR code.
                      </p>
                    </div>
                  )}

                  {qrInfo.status === 'connected' && (
                    <div className="space-y-1.5">
                      <p className="text-xs text-green-400/80">
                        WhatsApp is connected and ready to receive messages.
                      </p>
                      {qrInfo.lid && (
                        <p className="text-xs text-gray-400">
                          Assistant ID: <span className="font-mono text-gray-300">{qrInfo.lid}</span>
                          <span className="text-gray-600 ml-1">— search this in WhatsApp to find the assistant</span>
                        </p>
                      )}
                    </div>
                  )}

                  <p className="text-[10px] text-gray-600 mt-2">
                    Bridge ports: WS {qrInfo.ws_port} / HTTP {qrInfo.http_port}
                  </p>
                </div>
              )}

              {isRunning && !qrInfo && !qrError && (
                <p className="text-xs text-gray-600">Starting bridge...</p>
              )}
            </div>
          ) : (
            <div>
              <label className="block text-sm font-medium text-gray-300 mb-1.5">Bridge URL</label>
              <input
                value={bridgeUrl}
                onChange={(e) => updateBridgeUrl(e.target.value)}
                placeholder="ws://localhost:3001"
                className="input text-xs font-mono"
              />
              <p className="text-[10px] text-gray-600 mt-1">
                WebSocket URL of your external WhatsApp bridge.
              </p>
            </div>
          )}
        </div>
      )}
    </div>
  )
}
