import { useState, useEffect, useCallback } from 'react'
import { api, myApi } from '../../api'
import { useProfile } from '../../contexts/ProfileContext'
import type { SharedMetrics, SharedProviderMetrics, SharedPolicy } from '../../types'

interface Props {
  profileId: string
}

function statusBadge(m: SharedProviderMetrics, isFirst: boolean, policy: SharedPolicy) {
  if (m.error_rate >= 1.0 || m.consecutive_failures >= policy.failure_threshold) {
    return <span className="text-red-400 font-medium">Broken</span>
  }
  if (m.error_rate >= policy.error_rate_threshold) {
    return <span className="text-yellow-400 font-medium">Degraded</span>
  }
  if (m.latency_ema_ms > policy.latency_threshold_ms) {
    return <span className="text-yellow-400 font-medium">Slow</span>
  }
  if (isFirst) {
    return <span className="text-green-400 font-medium">Primary</span>
  }
  return <span className="text-blue-400 font-medium">Active</span>
}

function scoreColor(score: number, isFirst: boolean): string {
  if (isFirst) return 'text-green-400 font-medium'
  if (score < 0.3) return 'text-green-400/70'
  if (score < 0.6) return 'text-gray-300'
  return 'text-yellow-400'
}

function errorRateColor(rate: number): string {
  if (rate < 0.05) return 'text-green-400'
  if (rate < 0.3) return 'text-yellow-400'
  return 'text-red-400'
}

function formatLatency(ms: number): string {
  if (ms === 0) return '-'
  if (ms < 1000) return `${Math.round(ms)}ms`
  return `${(ms / 1000).toFixed(1)}s`
}

function timeAgo(isoDate: string): string {
  const diff = Math.floor((Date.now() - new Date(isoDate).getTime()) / 1000)
  if (diff < 5) return 'just now'
  if (diff < 60) return `${diff}s ago`
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`
  return `${Math.floor(diff / 3600)}h ago`
}

function PolicyBar({ policy }: { policy: SharedPolicy }) {
  return (
    <div className="bg-surface-dark/50 rounded-lg px-4 py-2.5 text-xs text-gray-400 flex flex-wrap gap-x-5 gap-y-1">
      <span>
        <span className="text-gray-500">Policy:</span>{' '}
        <span className="text-gray-200">Adaptive Routing</span>
      </span>
      <span>
        <span className="text-gray-500">Weights:</span>{' '}
        latency={policy.weight_latency} err={policy.weight_error_rate} priority={policy.weight_priority}
      </span>
      <span>
        <span className="text-gray-500">EMA:</span> α={policy.ema_alpha}
      </span>
      <span>
        <span className="text-gray-500">Circuit breaker:</span> {policy.failure_threshold} failures
      </span>
      <span>
        <span className="text-gray-500">Probe:</span> {(policy.probe_probability * 100).toFixed(0)}% every {policy.probe_interval_secs}s
      </span>
      <span>
        <span className="text-gray-500">Latency threshold:</span> {formatLatency(policy.latency_threshold_ms)}
      </span>
    </div>
  )
}

export default function ProviderQosTab({ profileId }: Props) {
  const { isOwn } = useProfile()
  const [metrics, setMetrics] = useState<SharedMetrics | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const fetchMetrics = useCallback(async () => {
    try {
      const data = isOwn
        ? await myApi.providerMetrics()
        : await api.providerMetrics(profileId)
      setMetrics(data)
      setError(null)
    } catch (e: any) {
      setError(e.message || 'Failed to load metrics')
    } finally {
      setLoading(false)
    }
  }, [profileId, isOwn])

  useEffect(() => {
    fetchMetrics()
    const interval = setInterval(fetchMetrics, 10_000)
    return () => clearInterval(interval)
  }, [fetchMetrics])

  if (loading) {
    return (
      <div className="flex items-center justify-center h-48">
        <div className="animate-spin w-5 h-5 border-2 border-accent border-t-transparent rounded-full" />
      </div>
    )
  }

  if (error) {
    return (
      <div className="text-center text-gray-400 py-12">
        <p className="text-lg mb-2">Unable to load metrics</p>
        <p className="text-sm">{error}</p>
      </div>
    )
  }

  if (!metrics || !metrics.providers || metrics.providers.length === 0) {
    return (
      <div className="text-center text-gray-400 py-12">
        <p className="text-lg mb-2">No metrics yet</p>
        <p className="text-sm">
          Provider QoS data will appear once the gateway processes requests
          with 2+ providers configured.
        </p>
      </div>
    )
  }

  const maxLatency = Math.max(...metrics.providers.map((p) => p.latency_ema_ms), 1)
  const totalRequests = metrics.providers.reduce((s, p) => s + p.success_count + p.failure_count, 0)
  const totalSuccess = metrics.providers.reduce((s, p) => s + p.success_count, 0)
  const totalFailure = metrics.providers.reduce((s, p) => s + p.failure_count, 0)

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h3 className="text-sm font-medium text-gray-300">Provider QoS Metrics</h3>
        <span className="text-xs text-gray-500">
          Updated {timeAgo(metrics.updated_at)}
        </span>
      </div>

      {metrics.policy && <PolicyBar policy={metrics.policy} />}

      <div className="overflow-x-auto">
        <table className="w-full text-sm">
          <thead>
            <tr className="text-gray-400 text-left border-b border-gray-700/50">
              <th className="py-2 px-3 font-medium">Provider</th>
              <th className="py-2 px-3 font-medium">Model</th>
              <th className="py-2 px-3 font-medium">Latency (EMA)</th>
              <th className="py-2 px-3 font-medium">p95</th>
              <th className="py-2 px-3 font-medium">Err%</th>
              <th className="py-2 px-3 font-medium">Reqs</th>
              <th className="py-2 px-3 font-medium">Score</th>
              <th className="py-2 px-3 font-medium">Status</th>
            </tr>
          </thead>
          <tbody>
            {metrics.providers.map((p, i) => {
              const barWidth = maxLatency > 0 ? Math.min((p.latency_ema_ms / maxLatency) * 100, 100) : 0
              return (
                <tr
                  key={`${p.provider}-${p.model}`}
                  className={`border-b border-gray-700/30 hover:bg-surface-dark/30 ${i === 0 ? 'bg-green-500/5' : ''}`}
                >
                  <td className="py-2.5 px-3 text-gray-200 font-medium">{p.provider}</td>
                  <td className="py-2.5 px-3 text-gray-400 font-mono text-xs">
                    {p.model.length > 20 ? p.model.slice(0, 18) + '..' : p.model}
                  </td>
                  <td className="py-2.5 px-3">
                    <div className="flex items-center gap-2">
                      <div className="w-20 h-2 bg-gray-700/50 rounded-full overflow-hidden">
                        <div
                          className="h-full rounded-full bg-accent/70"
                          style={{ width: `${barWidth}%` }}
                        />
                      </div>
                      <span className="text-gray-300 text-xs whitespace-nowrap">
                        {formatLatency(p.latency_ema_ms)}
                      </span>
                    </div>
                  </td>
                  <td className="py-2.5 px-3 text-gray-400 text-xs">
                    {formatLatency(p.p95_latency_ms)}
                  </td>
                  <td className={`py-2.5 px-3 text-xs ${errorRateColor(p.error_rate)}`}>
                    {(p.error_rate * 100).toFixed(1)}%
                  </td>
                  <td className="py-2.5 px-3 text-gray-400 text-xs">
                    {p.success_count + p.failure_count}
                  </td>
                  <td className={`py-2.5 px-3 text-xs ${scoreColor(p.score, i === 0)}`}>
                    {p.score.toFixed(3)}
                  </td>
                  <td className="py-2.5 px-3 text-xs">
                    {statusBadge(p, i === 0, metrics.policy)}
                  </td>
                </tr>
              )
            })}
          </tbody>
        </table>
      </div>

      <div className="flex gap-4 text-xs text-gray-500 pt-1">
        <span>{totalRequests} requests</span>
        <span className="text-green-400/60">{totalSuccess} ok</span>
        <span className="text-red-400/60">{totalFailure} failed</span>
      </div>
    </div>
  )
}
