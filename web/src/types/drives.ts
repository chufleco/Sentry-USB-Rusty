export interface DriveSummary {
  id: number
  date: string
  startTime: string
  endTime: string
  durationMs: number
  distanceMi: number
  distanceKm: number
  avgSpeedMph: number
  maxSpeedMph: number
  avgSpeedKmh: number
  maxSpeedKmh: number
  clipCount: number
  pointCount: number
  startPoint: [number, number] | null
  endPoint: [number, number] | null
  tags?: string[]
  fsdEngagedMs: number
  fsdDisengagements: number
  fsdAccelPushes: number
  fsdPercent: number
  fsdDistanceKm: number
  fsdDistanceMi: number
  autosteerEngagedMs: number
  autosteerPercent: number
  autosteerDistanceKm: number
  autosteerDistanceMi: number
  taccEngagedMs: number
  taccPercent: number
  taccDistanceKm: number
  taccDistanceMi: number
  assistedPercent: number
  batteryPctStart?: number
  batteryPctEnd?: number
  batteryPctUsed?: number
  interiorTempMinC?: number
  interiorTempMaxC?: number
  exteriorTempAvgC?: number
  hvacRuntimeS?: number
  tireFlPsi?: number
  tireFrPsi?: number
  tireRlPsi?: number
  tireRrPsi?: number
  odometerMiStart?: number
  odometerMiEnd?: number
  odometerMiDriven?: number
  startLocation?: string
  endLocation?: string
  source?: string
  externalSignature?: string
  tessieAutopilotPercent?: number
}

export interface FsdEvent {
  lat: number
  lng: number
  type: "disengagement" | "accel_push"
}

export interface DriveDetail extends Omit<DriveSummary, "startPoint" | "endPoint"> {
  points: [number, number, number, number][]
  gearStates?: number[]
  fsdStates?: number[]
  fsdEvents?: FsdEvent[]
}

export interface RouteOverview {
  id: number
  points: [number, number][]
  source?: string
}
