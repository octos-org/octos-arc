import { useState, useEffect, useCallback } from 'react'
import { api } from '../api'
import type { OverviewResponse, ProfileResponse } from '../types'

export function useOverview(pollInterval = 5000) {
  const [data, setData] = useState<OverviewResponse | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const refresh = useCallback(async () => {
    try {
      const res = await api.overview()
      setData(res)
      setError(null)
    } catch (e: any) {
      setError(e.message)
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    refresh()
    const timer = setInterval(refresh, pollInterval)
    return () => clearInterval(timer)
  }, [refresh, pollInterval])

  return { data, error, loading, refresh }
}

export function useProfile(id: string | undefined) {
  const [profile, setProfile] = useState<ProfileResponse | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const refresh = useCallback(async () => {
    if (!id) return
    try {
      const res = await api.getProfile(id)
      setProfile(res)
      setError(null)
    } catch (e: any) {
      setError(e.message)
    } finally {
      setLoading(false)
    }
  }, [id])

  useEffect(() => {
    refresh()
    const timer = setInterval(refresh, 5000)
    return () => clearInterval(timer)
  }, [refresh])

  return { profile, error, loading, refresh }
}
