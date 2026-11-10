import { createClient } from '@td/shared'

/** The engine's API, through main. */
export const api = createClient((request) => window.td.request(request))
