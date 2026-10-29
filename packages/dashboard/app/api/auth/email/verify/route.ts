import { verifyEmailCode } from "@/lib/identity/auth";
import { errorResponse } from "@/lib/server";

export const dynamic = "force-dynamic";

export async function POST(request: Request) {
  try {
    return await verifyEmailCode(request);
  } catch (error) {
    return errorResponse(error);
  }
}
