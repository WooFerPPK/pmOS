export class Resizable {
    constructor(element, zIndexManager, onResizeStart, config = {}) {
        this.element = element;
        this.zIndexManager = zIndexManager;
        this.onResizeStart = onResizeStart;

        const { minWidth = 400, minHeight = 300 } = config;
        this.minWidth = minWidth;
        this.minHeight = minHeight;

        this.createResizeHandle();

        this.startX = 0;
        this.startY = 0;
        this.startWidth = 0;
        this.startHeight = 0;
        this.dragging = false;

        // Binding methods to the current instance for consistent references.
        this.boundInitDrag = this.initDrag.bind(this);
        this.boundDoDrag = this.doDrag.bind(this);
        this.boundStopDrag = this.stopDrag.bind(this);
        
        this.handle.addEventListener('mousedown', this.boundInitDrag);
    }

    createResizeHandle() {
        this.handle = document.createElement('div');
        this.handle.className = 'resizable-handle';
        this.element.appendChild(this.handle);
    }

    initDrag(e) {
        e.preventDefault();
        // Call the onResizeStart callback if provided
        if (this.onResizeStart) {
            this.onResizeStart();
        }

        this.dragging = true;

        this.startX = e.clientX;
        this.startY = e.clientY;
        this.startWidth = this.element.offsetWidth;
        this.startHeight = this.element.offsetHeight;

        document.addEventListener('mousemove', this.boundDoDrag);
        document.addEventListener('mouseup', this.boundStopDrag);
    }

    doDrag(e) {
        if (!this.dragging) return;

        const newWidth = this.startWidth + e.clientX - this.startX;
        const newHeight = this.startHeight + e.clientY - this.startY;
        const maxWidth = window.innerWidth - this.element.offsetLeft;
        const maxHeight = window.innerHeight - this.element.offsetTop;
    
        const finalWidth = Math.min(Math.max(newWidth, this.minWidth), maxWidth);
        const finalHeight = Math.min(Math.max(newHeight, this.minHeight), maxHeight);
    
        this.element.style.width = finalWidth + 'px';
        this.element.style.height = finalHeight + 'px';
    }

    stopDrag() {
        this.dragging = false;
        document.removeEventListener('mousemove', this.boundDoDrag);
        document.removeEventListener('mouseup', this.boundStopDrag);
    }
}
