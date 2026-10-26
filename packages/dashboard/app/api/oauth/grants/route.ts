import { listGrants } from "@/lib/oauth/server";
import { errorResponse } from "@/lib/server";

export const dynamic = "force-dynamic";

export function GET(request: Request) {
  try {
    return listGrants(request);
  } catch (error) {
    return errorResponse(error);
  }
}
