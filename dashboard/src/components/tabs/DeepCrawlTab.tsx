import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

export default function DeepCrawlTab({ config, onChange }: Props) {
  const envVars = config.env_vars || {}

  const updateEnv = (key: string, value: string) => {
    onChange({ ...config, env_vars: { ...envVars, [key]: value } })
  }

  return (
    <div className="space-y-4">
      <p className="text-xs text-gray-500">
        Configure the deep web crawling tool for research tasks. The agent can crawl websites,
        extract content, and follow links to gather information.
      </p>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">Max Crawl Depth</label>
        <input
          value={envVars['CRAWL_MAX_DEPTH'] || ''}
          onChange={(e) => updateEnv('CRAWL_MAX_DEPTH', e.target.value)}
          placeholder="3"
          className="input max-w-[120px]"
        />
        <p className="text-xs text-gray-600 mt-1">Maximum link depth to follow from the starting URL (default: 3).</p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">Max Pages</label>
        <input
          value={envVars['CRAWL_MAX_PAGES'] || ''}
          onChange={(e) => updateEnv('CRAWL_MAX_PAGES', e.target.value)}
          placeholder="50"
          className="input max-w-[120px]"
        />
        <p className="text-xs text-gray-600 mt-1">Maximum number of pages to crawl per request (default: 50).</p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">Request Timeout (seconds)</label>
        <input
          value={envVars['CRAWL_TIMEOUT_SECS'] || ''}
          onChange={(e) => updateEnv('CRAWL_TIMEOUT_SECS', e.target.value)}
          placeholder="30"
          className="input max-w-[120px]"
        />
      </div>
    </div>
  )
}
