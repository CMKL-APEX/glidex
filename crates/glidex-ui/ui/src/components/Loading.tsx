export function Loading() {
  return (
    <div className="flex justify-center items-center py-12">
      <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-sky-600" />
      <span className="ml-3 text-gray-600">Loading...</span>
    </div>
  );
}

export function LoadingCard() {
  return (
    <div className="bg-white rounded-xl shadow-md p-6 border border-gray-100 animate-pulse">
      <div className="flex items-start justify-between">
        <div className="flex-1">
          <div className="h-6 bg-gray-200 rounded-sm w-1/3 mb-2" />
          <div className="h-4 bg-gray-200 rounded-sm w-2/3" />
        </div>
        <div className="h-6 bg-gray-200 rounded-full w-16" />
      </div>
      <div className="mt-4 grid grid-cols-2 gap-4">
        <div className="h-4 bg-gray-200 rounded-sm" />
        <div className="h-4 bg-gray-200 rounded-sm" />
      </div>
      <div className="mt-4 pt-4 border-t border-gray-100">
        <div className="flex space-x-2">
          <div className="h-8 bg-gray-200 rounded-sm w-16" />
          <div className="h-8 bg-gray-200 rounded-sm w-16" />
        </div>
      </div>
    </div>
  );
}
