import React from "react";
import { Info as InfoIcon } from "lucide-react";
import { Dialog, DialogContent, DialogTitle, DialogTrigger } from "./ui/dialog";
import { VisuallyHidden } from "./ui/visually-hidden";
import { About } from "./About";

interface InfoProps {
    isCollapsed: boolean;
    /** Square, label-less button sized to sit in the sidebar footer's icon row. */
    iconOnly?: boolean;
}

const Info = React.forwardRef<HTMLButtonElement, InfoProps>(({ isCollapsed, iconOnly = false }, ref) => {
  return (
    <Dialog aria-describedby={undefined}>
      <DialogTrigger asChild>
        <button 
          ref={ref} 
          className={`flex items-center justify-center cursor-pointer border-none transition-colors rounded-lg ${
            iconOnly
              ? "w-8 h-8 bg-gray-200 hover:bg-gray-300 shadow-sm"
              : isCollapsed 
                ? "mb-2 bg-transparent p-2 hover:bg-gray-100" 
                : "w-full mb-2 px-3 py-1.5 mt-1 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-200 shadow-sm"
          }`}
          title="About Meetily"
          aria-label="About Meetily"
        >
          <InfoIcon className={`text-gray-600 ${isCollapsed && !iconOnly ? "w-5 h-5" : "w-4 h-4"}`} />
          {!isCollapsed && !iconOnly && (
            <span className="ml-2 text-sm text-gray-700">About</span>
          )}
        </button>
      </DialogTrigger>
      <DialogContent>
        <VisuallyHidden>
          <DialogTitle>About Meetily</DialogTitle>
        </VisuallyHidden>
        <About />
      </DialogContent>
    </Dialog>
  );
});

Info.displayName = "About";

export default Info; 