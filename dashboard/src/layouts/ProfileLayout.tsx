import { Outlet } from 'react-router-dom'
import { ProfileProvider } from '../contexts/ProfileContext'

export default function ProfileLayout() {
  return (
    <ProfileProvider>
      <Outlet />
    </ProfileProvider>
  )
}
