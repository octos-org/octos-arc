interface Tab {
  key: string
  label: string
}

interface Props {
  tabs: Tab[]
  activeTab: string
  onTabChange: (key: string) => void
}

export default function CategoryTabs({ tabs, activeTab, onTabChange }: Props) {
  return (
    <div className="flex border-b border-gray-700/50 mb-6 overflow-x-auto">
      {tabs.map((tab) => (
        <button
          key={tab.key}
          type="button"
          onClick={() => onTabChange(tab.key)}
          className={`px-4 py-2.5 text-sm font-medium border-b-2 transition whitespace-nowrap ${
            activeTab === tab.key
              ? 'border-accent text-accent'
              : 'border-transparent text-gray-500 hover:text-gray-300'
          }`}
        >
          {tab.label}
        </button>
      ))}
    </div>
  )
}
