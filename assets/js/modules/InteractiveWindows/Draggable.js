export class Draggable {
    constructor(element, zIndexManager, bar) {
        this.element = element;
        this.zIndexManager = zIndexManager;
        this.bar = bar || element.querySelector('.draggable-bar');

        // Initial positions
        this.startX = 0;
        this.startY = 0;
        this.dragging = false;

        this.bar.addEventListener('mousedown', this.initDrag);
        this.bar.addEventListener('touchstart', this.initTouchDrag);
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
        this.dragging = true;

        const { clientX, clientY } = e;

        // Calculate offsets based on the element itself, not the target of the event
        const { left, top } = this.element.getBoundingClientRect();
        this.startX = clientX - left;
        this.startY = clientY - top;

        document.addEventListener('mousemove', this.doDrag);
        document.addEventListener('mouseup', this.stopDrag);
    }

    doDrag = (e) => {
        if (!this.dragging) return;
        this.updatePosition(e.clientX, e.clientY);
    }

    stopDrag = () => {
        this.dragging = false;
        document.removeEventListener('mousemove', this.doDrag);
        document.removeEventListener('mouseup', this.stopDrag);
    }

    initTouchDrag = (e) => {
        if (e.target.closest('.close-btn') || e.target.closest('.fullscreen-btn') || e.target.closest('.smallscreen-btn')) return;

        e.preventDefault();
        this.dragging = true;

        const touch = e.touches[0];

        // Calculate offsets based on the element itself, not the target of the event
        const { left, top } = this.element.getBoundingClientRect();
        this.startX = touch.clientX - left;
        this.startY = touch.clientY - top;

        document.addEventListener('touchmove', this.doTouchDrag);
        document.addEventListener('touchend', this.stopTouchDrag);
    }

    doTouchDrag = (e) => {
        if (!this.dragging) return;
        const touch = e.touches[0];
        this.updatePosition(touch.clientX, touch.clientY);
    }

    stopTouchDrag = () => {
        this.dragging = false;
        document.removeEventListener('touchmove', this.doTouchDrag);
        document.removeEventListener('touchend', this.stopTouchDrag);
    }
}
