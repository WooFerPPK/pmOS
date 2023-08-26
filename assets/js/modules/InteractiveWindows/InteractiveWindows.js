import { ZIndexManager } from './ZIndexManager.js';
import { Resizable } from './Resizeable.js';
import { WindowBar } from './WindowBar.js';
import { Draggable } from './Draggable.js';
import WindowManager from './WindowManager.js';

export class InteractiveWindows {
    constructor(config = {}) {
        const { minWidth = 330, minHeight = 370, observable} = config;
        this.observable = observable;
        this.minWidth = minWidth;
        this.minHeight = minHeight;
        
        this.windowManager = new WindowManager(this.observable);
        this.zIndexManager = new ZIndexManager();
        
        this.offsetIncrement = 30; // Offset for each new window
        this.currentOffset = 0; // Current accumulated offset for next window

        // Only listen to the window resize event at this point
        this.addResizeListener();
    }

    addWindow(win) {
        // Add a new window to the list and initialize it.
        this.windowManager.addWindow(win);
        this.setWindowStyles(win);
        this.windowBar = this.makeBar(win);
        win.style.zIndex = this.zIndexManager.getTopZIndex(); // Assign z-index for each new window
        this.makeResizable(win);
        this.makeDraggable(win);
        this.addFocusEffect(win);
    }


    setWindowStyles(win) {
        // Setting default size and position styles for each window.
        const windowWidth = this.clampWidth(window.innerWidth * 0.6);
        const windowHeight = this.clampHeight(window.innerHeight * 0.5);
        
        // Calculate horizontal and vertical centering, then add current offset for a cascading effect
        let centeredLeft = (window.innerWidth - windowWidth) / 2 + this.currentOffset;
        let centeredTop = (window.innerHeight - windowHeight) / 2 + this.currentOffset;
    
        // Check if the window with the current offset would exceed the viewport
        if (centeredLeft + windowWidth > window.innerWidth || centeredTop + windowHeight > window.innerHeight) {
            this.currentOffset = 0; // Reset offset if it exceeds viewport
            centeredLeft = 40; // Reset to 40px from the left
            centeredTop = 0;
        }
    
        Object.assign(win.style, {
            width: `${windowWidth}px`,
            height: `${windowHeight}px`,
            minWidth: `${this.minWidth}px`,
            minHeight: `${this.minHeight}px`,
            position: 'absolute',
            top: `${centeredTop}px`,
            left: `${centeredLeft}px`
        });
        
        // Increment the current offset for next window
        this.currentOffset += this.offsetIncrement;
    }

    makeBar(win) {
        // Making the window bar. Must be run before draggable.
        return new WindowBar(win, this.windowManager);
    }

    makeDraggable(win) {
        // Making window draggable
        new Draggable(win, this.zIndexManager, this.windowBar.bar);
    }

    makeResizable(win) {
        // Making window resizable
        new Resizable(win, this.zIndexManager, this.windowBar.forceExitFullScreen.bind(this.windowBar), { minWidth: 400, minHeight: 300 });
    }

    addFocusEffect(win) {
        win.addEventListener('mousedown', () => {
            win.style.zIndex = this.zIndexManager.getTopZIndex(); 
            // Inform WindowManager about the focused window
            this.windowManager.setFocusedWindow(win);
            
        });
    }

    addResizeListener() {
        // Listen to viewport resize to adjust the windows if they are out of view.
        window.addEventListener('resize', () => {
            this.adjustWindowsToFitViewport();
        });
    }


    adjustWindowsToFitViewport() {
        // For each window, adjust its size and position to ensure it's within the viewport.
        const allOpenWindows = this.windowManager.getOpenWindows();
        allOpenWindows.forEach(win => {
            const rect = win.getBoundingClientRect();
            
            win.style.width = this.clampWidth(rect.width) + 'px';
            win.style.height = this.clampHeight(rect.height) + 'px';
            win.style.left = this.adjustHorizontalPosition(rect);
            win.style.top = this.adjustVerticalPosition(rect);
        });
    }

    adjustHorizontalPosition(rect) {
        // Adjust the horizontal position of a window based on the viewport width.
        if (rect.right > window.innerWidth) {
            return `${window.innerWidth - rect.width}px`;
        } else if (rect.left < 0) {
            return '0px';
        } else {
            return rect.left + 'px';
        }
    }

    adjustVerticalPosition(rect) {
        // Adjust the vertical position of a window based on the viewport height.
        if (rect.bottom > window.innerHeight) {
            return `${window.innerHeight - rect.height}px`;
        } else if (rect.top < 0) {
            return '0px';
        } else {
            return rect.top + 'px';
        }
    }

    clampWidth(width) {
        // Ensure the width is between the minimum width and viewport width.
        return Math.min(Math.max(width, this.minWidth), window.innerWidth);
    }

    clampHeight(height) {
        // Ensure the height is between the minimum height and viewport height.
        return Math.min(Math.max(height, this.minHeight), window.innerHeight);
    }
}

