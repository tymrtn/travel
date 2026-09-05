import { page as currentPage } from '$app/state';

// Retained mail components have their own historical route contract. Those
// routes are not registered by standalone Travel's generated router types.
export const page = currentPage as Omit<typeof currentPage, 'params'> & {
  params: { account?: string; box?: string; uid?: string; draft?: string };
};
