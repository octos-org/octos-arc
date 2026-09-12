import { useState, useEffect, useRef } from 'react'

interface Props {
  logStreamUrl: string
}

export default function LogPanel({ logStreamUrl }: Props) {
  const [logs, setLogs] = useState<string[]>([])
  const logRef = useRef<HTMLDivElement>(null)
  const eventSourceRef = useRef<EventSource | null>(null)

  useEffect(() => {
    setLogs([])
    const es = new EventSource(logStreamUrl)
    es.onmessage = (event) => {
      setLogs((prev) => [...prev.slice(-500), event.data])
    }
    es.onerror = () => {
      es.close()
    }
    eventSourceRef.current = es

    return () => {
      es.close()
      eventSourceRef.current = null
    }
  }, [logStreamUrl])

  // Auto-scroll
  useEffect(() => {
    if (logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight
    }
  }, [logs])

  return (
    <div className="bg-surface-dark rounded-lg border border-gray-700/50 overflow-hidden">
      <div className="flex items-center justify-between px-4 py-2 border-b border-gray-700/50">
        <span className="text-xs text-gray-500 font-medium">Live Logs</span>
        <button
          onClick={() => setLogs([])}
          className="text-[10px] text-gray-600 hover:text-gray-400"
        >
          Clear
        </button>
      </div>
      <div
        ref={logRef}
        className="h-[calc(100vh-280px)] min-h-[300px] overflow-y-auto p-3 font-mono text-[11px] text-gray-400 leading-relaxed"
      >
        {logs.length === 0 ? (
          <span className="text-gray-600">Waiting for logs...</span>
        ) : (
          logs.map((line, i) => <div key={i}>{line}</div>)
        )}
      </div>
    </div>
  )
}
