import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

export default function PptConfigTab({ config, onChange }: Props) {
  const updateEnv = (key: string, value: string) => {
    const newEnvVars = { ...config.env_vars }
    if (value) {
      newEnvVars[key] = value
    } else {
      delete newEnvVars[key]
    }
    onChange({ ...config, env_vars: newEnvVars })
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">PPT Generation</p>
        <p>Configure settings for PowerPoint/presentation generation. The agent uses these when creating slide decks via the <code className="bg-gray-800 px-1 rounded">pptx</code> skill. Set a template directory to provide custom .pptx templates, and a default theme for consistent branding.</p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">
          PPT Template Directory
        </label>
        <input
          value={config.env_vars['PPT_TEMPLATE_DIR'] || ''}
          onChange={(e) => updateEnv('PPT_TEMPLATE_DIR', e.target.value)}
          placeholder="/path/to/ppt/templates"
          className="input text-xs"
        />
        <p className="text-[10px] text-gray-600 mt-1">PPT_TEMPLATE_DIR</p>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-300 mb-1.5">
          Default Theme
        </label>
        <input
          value={config.env_vars['PPT_DEFAULT_THEME'] || ''}
          onChange={(e) => updateEnv('PPT_DEFAULT_THEME', e.target.value)}
          placeholder="default"
          className="input text-xs"
        />
        <p className="text-[10px] text-gray-600 mt-1">PPT_DEFAULT_THEME</p>
      </div>
    </div>
  )
}
