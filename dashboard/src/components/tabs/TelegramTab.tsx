import type { ProfileConfig } from '../../types'

interface Props {
  config: ProfileConfig
  onChange: (config: ProfileConfig) => void
}

export default function TelegramTab({ config, onChange }: Props) {
  const channel = config.channels.find((c) => c.type === 'telegram')
  const enabled = !!channel

  const toggle = () => {
    if (enabled) {
      onChange({ ...config, channels: config.channels.filter((c) => c.type !== 'telegram') })
    } else {
      onChange({
        ...config,
        channels: [
          ...config.channels,
          { type: 'telegram', token_env: 'TELEGRAM_BOT_TOKEN', allowed_senders: '' },
        ],
      })
    }
  }

  const updateBotToken = (v: string) => {
    const newEnvVars = { ...config.env_vars }
    if (v) {
      newEnvVars['TELEGRAM_BOT_TOKEN'] = v
    } else {
      delete newEnvVars['TELEGRAM_BOT_TOKEN']
    }
    onChange({ ...config, env_vars: newEnvVars })
  }

  const updateAllowedSenders = (v: string) => {
    const channels = config.channels.map((c) =>
      c.type === 'telegram' ? { ...c, allowed_senders: v } : c
    )
    onChange({ ...config, channels })
  }

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 space-y-1.5 bg-surface-dark/50 rounded-lg p-3 border border-gray-700/50">
        <p className="font-medium text-gray-300">Telegram Bot</p>
        <p>Connect your gateway to Telegram as a bot. Supports text, photos, documents, voice messages, and vision (sends images to LLM).</p>
        <ol className="list-decimal list-inside space-y-0.5 text-gray-500">
          <li>Message <a href="https://t.me/BotFather" target="_blank" rel="noopener" className="text-accent hover:underline">@BotFather</a> on Telegram and create a bot with <code className="bg-gray-800 px-1 rounded">/newbot</code></li>
          <li>Copy the bot token and paste it below</li>
          <li>Get your user ID from <a href="https://t.me/userinfobot" target="_blank" rel="noopener" className="text-accent hover:underline">@userinfobot</a> to restrict access (optional)</li>
        </ol>
        <p className="text-gray-600">Uses long-polling (no webhook URL needed). Each profile can use a different bot token.</p>
      </div>

      <label className="flex items-center gap-2 cursor-pointer">
        <input
          type="checkbox"
          checked={enabled}
          onChange={toggle}
          className="w-4 h-4 rounded bg-surface-dark border-gray-600 text-accent focus:ring-accent"
        />
        <span className="text-sm text-gray-300">Enable Telegram channel</span>
      </label>

      {enabled && (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">Bot Token</label>
            <input
              type="password"
              value={config.env_vars['TELEGRAM_BOT_TOKEN'] || ''}
              onChange={(e) => updateBotToken(e.target.value)}
              placeholder="123456:ABC-DEF..."
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Get this from @BotFather after creating your bot. Stored as TELEGRAM_BOT_TOKEN.
            </p>
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-300 mb-1.5">
              Allowed Senders
            </label>
            <input
              value={(channel as any)?.allowed_senders || ''}
              onChange={(e) => updateAllowedSenders(e.target.value)}
              placeholder="Telegram user IDs, comma-separated (empty = allow all)"
              className="input text-xs font-mono"
            />
            <p className="text-[10px] text-gray-600 mt-1">
              Comma-separated Telegram user IDs (numeric). Leave empty to allow anyone. Get your ID from @userinfobot.
            </p>
          </div>
        </>
      )}
    </div>
  )
}
