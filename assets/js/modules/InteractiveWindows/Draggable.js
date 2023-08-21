export class Draggable {
    constructor(element, zIndexManager, bar, dragDelay = 0) {
        this.element = element;
        this.zIndexManager = zIndexManager;
        this.bar = bar;

        // Initial positions
        this.startX = 0;
        this.startY = 0;
        this.dragDelay = dragDelay; // Drag Delay time
        this.dragging = false;

        this.bar.addEventListener('mousedown', this.initDrag);
        this.bar.addEventListener('touchstart', this.initDrag);
    }

    updatePosition = (clientX, clientY) => {
        let top = clientY - this.startY;
        let left = clientX - this.startX;

        // Constraints for top and left positions:
        top = Math.max(0, Math.min(top, window.innerHeight - this.element.offsetHeight));
        left = Math.max(0, Math.min(left, window.innerWidth - this.element.offsetWidth));

        this.element.style.position = 'absolute';
        this.element.style.top = `${top}px`;
        this.element.style.left = `${left}px`;
    }

    initDrag = (e) => {
        if (e.target.closest('.close-btn') || e.target.closest('.fullscreen-btn') || e.target.closest('.smallscreen-btn')) return;
    
        e.preventDefault();

        let dragTimeout;
        
        const initiateDragging = () => {
            this.dragging = true;
            this.bar.classList.add('dragging');
            
            let clientX = e.clientX;
            let clientY = e.clientY;
            
            if (e.touches) {
                clientX = e.touches[0].clientX;
                clientY = e.touches[0].clientY;
            }
    
            // Calculate offsets based on the element itself, not the target of the event
            const { left, top } = this.element.getBoundingClientRect();
            this.startX = clientX - left;
            this.startY = clientY - top;
    
            document.addEventListener('mousemove', this.doDrag);
            document.addEventListener('mouseup', this.stopDrag);

            document.addEventListener('touchmove', this.doDrag);
            document.addEventListener('touchend', this.stopDrag);
        };
    
        dragTimeout = setTimeout(initiateDragging, this.dragDelay);
    
        // Temporary mouseup listener to clear the timeout if mouseup happens before dragging starts
        const tempMouseUp = () => {
            clearTimeout(dragTimeout);
            document.removeEventListener('mouseup', tempMouseUp);
            document.addEventListener('touchend', this.stopDrag);
        };
    
        document.addEventListener('mouseup', tempMouseUp);
        document.addEventListener('touchend', this.stopDrag);
    }
    

    doDrag = (e) => {
        if (!this.dragging) return;
        if (e.touches) {
            const touch = e.touches[0];
            this.updatePosition(touch.clientX, touch.clientY);
        } else {
            this.updatePosition(e.clientX, e.clientY);
        }
    }

    stopDrag = () => {
        document.removeEventListener('mousemove', this.doDrag);
        document.removeEventListener('mouseup', this.stopDrag);

        document.removeEventListener('touchmove', this.doDrag);
        document.removeEventListener('touchend', this.stopDrag);
        setTimeout(() => { 
            this.dragging = false; 
            this.bar.classList.remove('dragging');
        }, this.dragDelay);
    }
}
