import { MobileNotificationsSection } from "@/components/settings/sections/MobileNotificationsSection"
import { CommunityFeaturesSection } from "@/components/settings/sections/CommunityFeaturesSection"

export function NotificationsTab() {
  // Only two cards — a balanced 2-column grid fills the row evenly instead
  // of leaving a wide empty gutter (matches the WiFi/Ethernet + Away/Cloud
  // rows on the Car & Network tab). Collapses to one column on mobile.
  return (
    <div className="grid items-start gap-2.5 sm:grid-cols-2">
      <MobileNotificationsSection />
      <CommunityFeaturesSection />
    </div>
  )
}
