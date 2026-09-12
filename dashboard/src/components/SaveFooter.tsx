interface Props {
  onSave: () => void
  saving: boolean
}

export default function SaveFooter({ onSave, saving }: Props) {
  return (
    <div className="px-5 py-4 border-t border-gray-700/50 flex justify-end mt-6">
      <button
        onClick={onSave}
        disabled={saving}
        className="px-6 py-2 text-sm font-medium rounded-lg bg-accent text-white hover:bg-accent-light transition disabled:opacity-50"
      >
        {saving ? 'Saving...' : 'Save Changes'}
      </button>
    </div>
  )
}
