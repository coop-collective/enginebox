import { version } from '../release';
export function GET() { return Response.json({ version }, { headers: { 'cache-control': 'no-store' } }); }
